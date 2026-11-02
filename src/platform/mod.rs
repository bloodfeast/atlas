//! Platform-specific code, behind `cfg`, each backend an owned FFI binding
//! with no crate.
//!
//! [`Backend`] watches one root for [`crate::watch`]:
//!
//! * Windows: `ReadDirectoryChangesW` on one thread per root.
//! * Linux: inotify, one watch per directory, on one thread per root.
//! * macOS: FSEvents with per-file events, one stream per root on a thread
//!   that runs its own CFRunLoop.
//! * Anything else: no backend; every root falls back to the stat walk.
//!
//! The API every backend offers: `Backend::spawn(root, index, sink)`
//! returns `None` when the root cannot be watched, and `Backend::stop`
//! ends the watch.
#![allow(unsafe_code)]

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::Backend;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::Backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::Backend;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod none {
    use std::path::Path;
    use std::sync::Arc;

    use crate::watch::Sink;

    /// No change notification on this platform: every root walks.
    #[derive(Debug)]
    pub(crate) struct Backend;

    impl Backend {
        pub(crate) fn spawn(_root: &Path, _index: usize, _sink: Arc<dyn Sink>) -> Option<Backend> {
            None
        }

        pub(crate) fn stop(&mut self) {}
    }
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(crate) use none::Backend;

/// Child-process control for the code indexer: lowering a child's
/// priority and ending one that ran past its timeout. Owned FFI, no crate.
pub(crate) mod process {
    #[cfg(unix)]
    mod sys {
        use std::os::raw::{c_int, c_uint};

        extern "C" {
            fn setpriority(which: c_int, who: c_uint, prio: c_int) -> c_int;
            fn kill(pid: c_int, sig: c_int) -> c_int;
        }

        /// `PRIO_PROCESS` is 0 on Linux and macOS.
        const PRIO_PROCESS: c_int = 0;
        const SIGKILL: c_int = 9;

        pub(crate) fn lower_priority(pid: u32) {
            // SAFETY: setpriority takes plain integers and only reads them;
            // a pid that no longer exists makes it fail with ESRCH.
            unsafe {
                setpriority(PRIO_PROCESS, pid as c_uint, 10);
            }
        }

        pub(crate) fn terminate(pid: u32) {
            // SAFETY: kill takes plain integers; the pid is our own child,
            // which has not been reaped (the waiter holds its Child), so the
            // number cannot have been reused.
            unsafe {
                kill(pid as c_int, SIGKILL);
            }
        }
    }

    #[cfg(windows)]
    mod sys {
        use std::ffi::c_void;

        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn TerminateProcess(h: *mut c_void, code: u32) -> i32;
            fn CloseHandle(h: *mut c_void) -> i32;
        }

        const PROCESS_TERMINATE: u32 = 0x0001;

        #[link(name = "kernel32")]
        extern "system" {
            fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
            fn Process32FirstW(snap: *mut c_void, entry: *mut ProcessEntry) -> i32;
            fn Process32NextW(snap: *mut c_void, entry: *mut ProcessEntry) -> i32;
        }

        const TH32CS_SNAPPROCESS: u32 = 0x2;

        /// `PROCESSENTRY32W`.
        #[repr(C)]
        struct ProcessEntry {
            size: u32,
            usage: u32,
            pid: u32,
            heap: usize,
            module: u32,
            threads: u32,
            parent: u32,
            priority: i32,
            flags: u32,
            exe: [u16; 260],
        }

        /// Every process as (pid, parent pid, executable name).
        pub(crate) fn processes() -> Vec<(u32, u32, String)> {
            let mut out = Vec::new();
            // SAFETY: the snapshot handle is checked, used only here and
            // closed once. The entry is a plain C struct, zeroed, with its
            // size field set as the API requires before the first call;
            // the name is read up to its NUL inside the fixed array.
            unsafe {
                let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snap.is_null() || snap as isize == -1 {
                    return out;
                }
                let mut e: ProcessEntry = std::mem::zeroed();
                e.size = std::mem::size_of::<ProcessEntry>() as u32;
                let mut ok = Process32FirstW(snap, &mut e);
                while ok != 0 {
                    let n = e.exe.iter().position(|&c| c == 0).unwrap_or(e.exe.len());
                    out.push((e.pid, e.parent, String::from_utf16_lossy(&e.exe[..n])));
                    ok = Process32NextW(snap, &mut e);
                }
                CloseHandle(snap);
            }
            out
        }

        /// Priority is set at creation on Windows (`BELOW_NORMAL_PRIORITY_CLASS`).
        pub(crate) fn lower_priority(_pid: u32) {}

        pub(crate) fn terminate(pid: u32) {
            // SAFETY: OpenProcess returns null or a handle we own; the handle
            // is used once and closed. The pid is our own child, still
            // unreaped (its Child is held by the waiter), so it is not reused.
            unsafe {
                let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !h.is_null() {
                    TerminateProcess(h, 1);
                    CloseHandle(h);
                }
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    mod sys {
        pub(crate) fn lower_priority(_pid: u32) {}
        pub(crate) fn terminate(_pid: u32) {}
    }

    /// Every process as (pid, parent pid, executable name), from `/proc`.
    #[cfg(target_os = "linux")]
    fn processes() -> Vec<(u32, u32, String)> {
        let mut out = Vec::new();
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            // `stat` is "pid (comm) state ppid ...": comm may hold spaces
            // and parentheses, so split at the last ')'.
            let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
            let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else { continue };
            let name = stat[open + 1..close].to_string();
            let ppid = stat[close + 1..].split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            out.push((pid, ppid, name));
        }
        out
    }

    /// Every process as (pid, parent pid, executable name), from `ps`
    /// (no stable FFI for the parent pid without a large struct layout).
    #[cfg(target_os = "macos")]
    fn processes() -> Vec<(u32, u32, String)> {
        let Ok(o) = std::process::Command::new("ps").args(["-Ao", "pid=,ppid=,comm="]).stderr(std::process::Stdio::null()).output() else { return Vec::new() };
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let pid = it.next()?.parse().ok()?;
                let ppid = it.next()?.parse().ok()?;
                let comm: Vec<&str> = it.collect();
                let comm = comm.join(" ");
                Some((pid, ppid, comm.rsplit('/').next().unwrap_or("").to_string()))
            })
            .collect()
    }

    #[cfg(windows)]
    use sys::processes;

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    fn processes() -> Vec<(u32, u32, String)> {
        Vec::new()
    }

    /// Whether the process list holds a `cargo` that is not `me` or one of
    /// its ancestors (atlas started by `cargo run` or `cargo test` must not
    /// wait for itself).
    ///
    /// # Arguments
    ///
    /// * `procs` - (pid, parent pid, executable name) of every process.
    /// * `me` - this process's pid.
    pub(crate) fn foreign_cargo_in(procs: &[(u32, u32, String)], me: u32) -> bool {
        let parent: std::collections::HashMap<u32, u32> = procs.iter().map(|p| (p.0, p.1)).collect();
        let mut mine = std::collections::HashSet::new();
        let mut at = me;
        // Bounded: a pid cycle (reuse races) cannot loop forever.
        while mine.insert(at) && mine.len() < 256 {
            match parent.get(&at) {
                Some(&p) if p != 0 && p != at => at = p,
                _ => break,
            }
        }
        procs.iter().any(|(pid, _, name)| {
            let n = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name);
            n.eq_ignore_ascii_case("cargo") && !mine.contains(pid)
        })
    }

    /// Whether a `cargo` process other than this one's ancestors runs.
    pub(crate) fn foreign_cargo_running() -> bool {
        foreign_cargo_in(&processes(), std::process::id())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cargo_ancestors_do_not_count_but_others_do() {
            let p = |a: u32, b: u32, n: &str| (a, b, n.to_string());
            let mine = [p(1, 0, "init"), p(10, 1, "cargo.exe"), p(11, 10, "atlas")];
            assert!(!foreign_cargo_in(&mine, 11));
            let other = [p(1, 0, "init"), p(10, 1, "cargo"), p(11, 10, "atlas"), p(20, 1, "Cargo.exe")];
            assert!(foreign_cargo_in(&other, 11));
            assert!(!foreign_cargo_in(&[p(1, 0, "init"), p(5, 1, "cargo-watch")], 1));
        }

        #[test]
        fn the_process_list_holds_this_process() {
            if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
                let me = std::process::id();
                assert!(processes().iter().any(|p| p.0 == me));
            }
        }
    }

    /// Lowers a child process's scheduling priority (nice 10 on Unix; a
    /// no-op on Windows, where the child is created below normal).
    pub(crate) fn lower_priority(pid: u32) {
        sys::lower_priority(pid)
    }

    /// Ends a child process at once.
    pub(crate) fn terminate(pid: u32) {
        sys::terminate(pid)
    }
}

/// The name a directory entry carries on disk, read from the directory
/// itself rather than through a handle to the file.
pub(crate) mod names {
    use std::io;
    use std::path::Path;

    /// The on-disk spelling of `name` inside `dir`.
    ///
    /// On Windows the file system ignores case (and accepts 8.3 short
    /// names), so the stored spelling is read from the directory entry with
    /// `FindFirstFileExW`. That opens no handle to the file, so a file
    /// another process is replacing by rename still answers with its
    /// current entry. Elsewhere the name is returned unchanged.
    ///
    /// # Arguments
    ///
    /// * `dir` - the folder that holds the entry.
    /// * `name` - one path component, with no wildcard characters.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::NotFound`] if no entry matches; other I/O errors as
    /// the system reports them.
    #[cfg(windows)]
    pub(crate) fn on_disk(dir: &Path, name: &str) -> io::Result<String> {
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;

        /// `WIN32_FIND_DATAW`.
        #[repr(C)]
        struct FindData {
            attributes: u32,
            times: [u32; 6],
            size_high: u32,
            size_low: u32,
            reserved: [u32; 2],
            name: [u16; 260],
            alt_name: [u16; 14],
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn FindFirstFileExW(name: *const u16, level: i32, data: *mut FindData, op: i32, filter: *mut c_void, flags: u32) -> *mut c_void;
            fn FindClose(h: *mut c_void) -> i32;
        }
        const FIND_EX_INFO_BASIC: i32 = 1;
        const FIND_EX_SEARCH_NAME_MATCH: i32 = 0;

        let wide: Vec<u16> = dir.join(name).as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; `data` is
        // a zeroed plain C struct of the documented layout that the call
        // fills; the returned handle is checked and closed once. The name is
        // read up to its NUL inside the fixed array.
        unsafe {
            let mut data: FindData = std::mem::zeroed();
            let h = FindFirstFileExW(wide.as_ptr(), FIND_EX_INFO_BASIC, &mut data, FIND_EX_SEARCH_NAME_MATCH, std::ptr::null_mut(), 0);
            if h as isize == -1 {
                return Err(io::Error::last_os_error());
            }
            FindClose(h);
            let n = data.name.iter().position(|&c| c == 0).unwrap_or(data.name.len());
            String::from_utf16(&data.name[..n]).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file name is not UTF-16"))
        }
    }

    /// The on-disk spelling of `name` inside `dir`: on this platform, `name`.
    #[cfg(not(windows))]
    pub(crate) fn on_disk(_dir: &Path, name: &str) -> io::Result<String> {
        Ok(name.to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_differently_cased_request_gets_the_stored_name() {
            let dir = std::env::temp_dir().join(format!("atlas-names-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Stored Name.md"), b"x").unwrap();
            assert_eq!(on_disk(&dir, "Stored Name.md").unwrap(), "Stored Name.md");
            if cfg!(windows) {
                assert_eq!(on_disk(&dir, "stored name.MD").unwrap(), "Stored Name.md");
                assert_eq!(on_disk(&dir, "missing.md").unwrap_err().kind(), io::ErrorKind::NotFound);
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
