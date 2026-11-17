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
//! returns `None` when the root cannot be watched, and otherwise returns
//! only once the watch is armed (a change made after it returns is
//! reported); `Backend::stop` ends the watch.
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

pub(crate) mod boot;
pub(crate) mod registry;
pub(crate) mod shutdown;

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
            // SAFETY: kill takes plain integers and only reads them. The pid
            // is our own child, but this is a race, not a guarantee: if the
            // child exits at the deadline, the waiter may reap it just before
            // this call, and in that window of microseconds the number could
            // name a new process. The caller accepts that window.
            unsafe {
                kill(pid as c_int, SIGKILL);
            }
        }

        /// Whether process `pid` exists: `kill(pid, 0)` succeeds, or fails
        /// with EPERM (it exists but is another user's). ESRCH is gone.
        pub(crate) fn alive(pid: u32) -> bool {
            let Ok(p) = c_int::try_from(pid) else { return false };
            if p <= 0 {
                return false;
            }
            // SAFETY: kill with signal 0 sends nothing; it takes plain
            // integers and only checks that the process exists.
            let r = unsafe { kill(p, 0) };
            r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
        }

        /// EPERM is 1 on Linux and macOS.
        const EPERM: c_int = 1;

        /// Kills the process group `pid` led (L5): the leader may be gone
        /// and reaped, but the group id stays taken while a member lives.
        pub(crate) fn terminate_group(pid: u32) {
            let Ok(p) = c_int::try_from(pid) else { return };
            if p <= 1 {
                return;
            }
            // SAFETY: kill takes plain integers and only reads them; a
            // negative pid names the group, refused for 0 and 1 above.
            unsafe {
                kill(-p, SIGKILL);
            }
        }

        /// Kills the process group `pid` leads (a child started with
        /// [`super::own_group`]), then `pid` itself.
        pub(crate) fn terminate_tree(pid: u32) {
            let Ok(p) = c_int::try_from(pid) else { return };
            if p <= 1 {
                return;
            }
            // SAFETY: kill takes plain integers and only reads them. A
            // negative pid names the process group; `p` is our own unreaped
            // child, which leads its group, so the group cannot be reused
            // while it lives. A pid of 0 or 1 is refused above.
            unsafe {
                kill(-p, SIGKILL);
                kill(p, SIGKILL);
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
            fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
            fn GetLastError() -> u32;
        }

        const STILL_ACTIVE: u32 = 259;
        const ERROR_ACCESS_DENIED: u32 = 5;

        /// Whether process `pid` is running: it opens and has no exit code
        /// yet, or it exists but may not be opened (access denied).
        pub(crate) fn alive(pid: u32) -> bool {
            // SAFETY: OpenProcess returns null or a handle we own, used once
            // and closed; the exit code is a plain u32 that lives across the
            // call. GetLastError only reads this thread's last error.
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if h.is_null() {
                    return GetLastError() == ERROR_ACCESS_DENIED;
                }
                let mut code = 0u32;
                let ok = GetExitCodeProcess(h, &mut code);
                CloseHandle(h);
                ok != 0 && code == STILL_ACTIVE
            }
        }

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
            // is used once and closed. The pid is our own child, but if it
            // exits at the deadline the waiter may close its handle just
            // before this call, and in that window of microseconds the number
            // could name a new process. The caller accepts that window.
            unsafe {
                let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !h.is_null() {
                    TerminateProcess(h, 1);
                    CloseHandle(h);
                }
            }
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn GetProcessTimes(h: *mut c_void, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
        }

        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

        /// When process `pid` was created (100 ns ticks), if it can be read.
        pub(crate) fn created(pid: u32) -> Option<u64> {
            // SAFETY: OpenProcess returns null or a handle we own, used once
            // and closed; the four out-pointers are plain u64s (FILETIME is
            // two u32s, the same size and alignment as one u64 here) that
            // live across the call.
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if h.is_null() {
                    return None;
                }
                let (mut c, mut e, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
                let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
                CloseHandle(h);
                (ok != 0).then_some(c)
            }
        }

        /// Ends `pid` and every process descended from it. Windows has no
        /// process group to kill at once, so the tree is read from one
        /// snapshot first (a grandchild whose parent died first would lose
        /// its parent link) and then each is ended, the child last. A
        /// process's parent id can be stale (its parent died and the number
        /// was reused), so a process counts as a child only when it was
        /// created after the parent it names; one whose times cannot be read
        /// is left alone.
        pub(crate) fn terminate_tree(pid: u32) {
            let procs = processes();
            let Some(born) = created(pid) else {
                terminate(pid);
                return;
            };
            let mut tree = vec![(pid, born)];
            let mut i = 0;
            while i < tree.len() && tree.len() < 4096 {
                let (at, at_born) = tree[i];
                let kids: Vec<(u32, u64)> = procs
                    .iter()
                    .filter(|p| p.1 == at && p.0 != at && p.0 != 0 && !tree.iter().any(|t| t.0 == p.0))
                    .filter_map(|p| created(p.0).filter(|c| *c >= at_born).map(|c| (p.0, c)))
                    .collect();
                tree.extend(kids);
                i += 1;
            }
            for p in tree.iter().rev() {
                terminate_if_born(p.0, p.1);
            }
        }

        /// Ends process `pid` only if it is still the one created at `born`:
        /// one handle, opened both to read its times and to end it, so a pid
        /// reused between the check and the kill cannot be hit (L6). True
        /// when it was ended.
        pub(crate) fn terminate_if_born(pid: u32, born: u64) -> bool {
            // SAFETY: OpenProcess returns null or a handle we own, used here
            // and closed once; the out-pointers are plain u64s (FILETIME is
            // two u32s, the same size and alignment as one u64 here) that
            // live across the call.
            unsafe {
                let h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if h.is_null() {
                    return false;
                }
                let (mut c, mut e, mut k, mut u) = (0u64, 0u64, 0u64, 0u64);
                let same = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 && c == born;
                let ended = same && TerminateProcess(h, 1) != 0;
                CloseHandle(h);
                ended
            }
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> *mut c_void;
            fn SetInformationJobObject(job: *mut c_void, class: i32, info: *mut c_void, len: u32) -> i32;
            fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
        }

        /// `JOBOBJECT_BASIC_LIMIT_INFORMATION`.
        #[repr(C)]
        #[derive(Default)]
        struct BasicLimits {
            per_process_user_time: i64,
            per_job_user_time: i64,
            flags: u32,
            min_working_set: usize,
            max_working_set: usize,
            active_processes: u32,
            affinity: usize,
            priority_class: u32,
            scheduling_class: u32,
        }

        /// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
        #[repr(C)]
        #[derive(Default)]
        struct ExtendedLimits {
            basic: BasicLimits,
            io: [u64; 6],
            process_memory: usize,
            job_memory: usize,
            peak_process_memory: usize,
            peak_job_memory: usize,
        }

        const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
        const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

        /// A job object that ends every process in it when its last handle
        /// closes. atlas holds the one handle, so the processes go when the
        /// Tie is dropped or when atlas ends, killed hard or not.
        pub(crate) struct Tie(*mut c_void);

        // SAFETY: the handle is a kernel object handle, usable from any
        // thread; only Drop closes it, once.
        unsafe impl Send for Tie {}

        impl Drop for Tie {
            fn drop(&mut self) {
                // SAFETY: the handle is ours and closed only here; closing
                // the last handle ends every process in the job.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }

        /// Puts `child` in a new kill-on-close job. A process the child
        /// started before this call is not in it (the tree kill covers it).
        pub(crate) fn tie(child: &std::process::Child) -> Option<Tie> {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: the job handle is checked and owned by the Tie (closed
            // on its drop, also on the failure paths); the limits struct is
            // a plain C struct that lives across the call, its size passed;
            // the child's handle is borrowed from a live Child.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let tie = Tie(job);
                let mut info = ExtendedLimits::default();
                info.basic.flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    job,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    (&raw mut info).cast(),
                    std::mem::size_of::<ExtendedLimits>() as u32,
                );
                if set == 0 || AssignProcessToJobObject(job, child.as_raw_handle().cast()) == 0 {
                    return None;
                }
                Some(tie)
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    mod sys {
        pub(crate) fn lower_priority(_pid: u32) {}
        pub(crate) fn terminate(_pid: u32) {}
        pub(crate) fn terminate_tree(_pid: u32) {}
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

    /// This process's parent pid, when it can be read.
    pub(crate) fn parent() -> Option<u32> {
        #[cfg(unix)]
        {
            Some(std::os::unix::process::parent_id())
        }
        #[cfg(windows)]
        {
            let me = std::process::id();
            processes().into_iter().find(|p| p.0 == me).map(|p| p.1)
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    }

    /// Whether a `cargo` process other than this one's ancestors runs.
    pub(crate) fn foreign_cargo_running() -> bool {
        foreign_cargo_in(&processes(), std::process::id())
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

    /// Starts `cmd` in a process group of its own: a Ctrl-C at the terminal
    /// reaches atlas only, which then stops the child itself, and on Unix
    /// [`terminate_tree`] can end the group at once (`setpgid` through the
    /// standard library; `CREATE_NEW_PROCESS_GROUP` on Windows).
    pub(crate) fn own_group(cmd: &mut std::process::Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::raw::{c_int, c_ulong};
            use std::os::unix::process::CommandExt;
            extern "C" {
                fn prctl(option: c_int, arg2: c_ulong, arg3: c_ulong, arg4: c_ulong, arg5: c_ulong) -> c_int;
                fn getppid() -> c_int;
                fn _exit(code: c_int) -> !;
            }
            const PR_SET_PDEATHSIG: c_int = 1;
            const SIGKILL: c_ulong = 9;
            // L2: the child is killed when the thread that spawned it ends
            // (atlas killed hard included). Read before the fork: if atlas
            // died between the fork and the prctl, the parent is already
            // someone else, so the child leaves at once.
            let parent = c_int::try_from(std::process::id()).unwrap_or(0);
            // SAFETY: the closure runs in the child between fork and exec and
            // calls only async-signal-safe functions (prctl, getppid,
            // _exit) on plain integers; it allocates nothing.
            unsafe {
                cmd.pre_exec(move || {
                    prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0);
                    if getppid() != parent {
                        _exit(1);
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        #[cfg(not(any(unix, windows)))]
        let _ = cmd;
    }

    /// Ends a child started with [`own_group`] and everything it started (a
    /// `gh` extension runs as gh's own child, which a kill of gh alone would
    /// leave running): the process group on Unix, the process tree on
    /// Windows.
    pub(crate) fn terminate_tree(pid: u32) {
        sys::terminate_tree(pid)
    }

    /// Kills what is left of the process group `pid` led ([`own_group`]),
    /// after the leader exited by itself.
    #[cfg(unix)]
    pub(crate) fn terminate_group(pid: u32) {
        sys::terminate_group(pid)
    }

    /// Whether process `pid` exists (one atlas may not open counts).
    pub(crate) fn alive(pid: u32) -> bool {
        #[cfg(any(unix, windows))]
        {
            sys::alive(pid)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = pid;
            true
        }
    }

    /// When process `pid` started, where that is cheap to read: Windows, its
    /// creation time; Linux, field 22 of `/proc/PID/stat`. Else `None`.
    pub(crate) fn start_time(pid: u32) -> Option<u64> {
        #[cfg(windows)]
        {
            sys::created(pid)
        }
        #[cfg(target_os = "linux")]
        {
            let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // Fields after the `(comm)` start at field 3, so 22 is the 20th.
            s[s.rfind(')')? + 1..].split_whitespace().nth(19)?.parse().ok()
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = pid;
            None
        }
    }

    /// What ties a child's processes to atlas's life (L2): on Windows a
    /// kill-on-close job, which also ends the child's descendants when it is
    /// dropped. On Linux the child's `PR_SET_PDEATHSIG` ([`own_group`]) does
    /// it and this holds nothing; on macOS nothing does (a known leak).
    pub(crate) struct Tie {
        #[cfg(windows)]
        _job: Option<sys::Tie>,
    }

    impl Tie {
        /// Lets go of the tie now: on Windows the job closes, which ends
        /// whatever is still in it; elsewhere it does nothing.
        pub(crate) fn end(self) {
            #[cfg(windows)]
            drop(self._job);
        }
    }

    /// Ties `child`, started with [`own_group`], to atlas ([`Tie`]).
    pub(crate) fn tie(child: &std::process::Child) -> Tie {
        #[cfg(windows)]
        {
            Tie { _job: sys::tie(child) }
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Tie {}
        }
    }

    /// See the Windows `sys::terminate_if_born`.
    #[cfg(all(windows, test))]
    pub(crate) fn terminate_if_born(pid: u32, born: u64) -> bool {
        sys::terminate_if_born(pid, born)
    }

    /// Whether process `pid` is alive (a Linux zombie is not).
    #[cfg(test)]
    pub(crate) fn running(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| s.rfind(')').and_then(|c| s[c + 1..].split_whitespace().next().map(|st| st != "Z" && st != "X")))
                .unwrap_or(false)
        }
        #[cfg(not(target_os = "linux"))]
        {
            processes().iter().any(|p| p.0 == pid)
        }
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

/// A Ctrl-C (or, on Unix, a `SIGTERM` or `SIGHUP`) asks a long-running
/// command line process to stop, so `atlas pr-events serve` can stop its
/// forwarders and clean up their hooks before it exits. The handler only
/// sets a flag; the caller watches [`interrupt::requested`].
pub(crate) mod interrupt {
    use std::sync::atomic::{AtomicBool, Ordering};

    static REQUESTED: AtomicBool = AtomicBool::new(false);

    /// Whether a stop was asked for since [`install`].
    pub(crate) fn requested() -> bool {
        REQUESTED.load(Ordering::SeqCst)
    }

    /// Asks for a stop as a signal would (the tests' seam).
    #[cfg(test)]
    pub(crate) fn raise() {
        REQUESTED.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    extern "C" fn on_signal(_sig: std::os::raw::c_int) {
        // An atomic store is async-signal-safe.
        REQUESTED.store(true, Ordering::SeqCst);
    }

    /// Installs the handler (once per process is enough; again is harmless).
    #[cfg(unix)]
    pub(crate) fn install() {
        use std::os::raw::c_int;
        extern "C" {
            fn signal(sig: c_int, handler: usize) -> usize;
        }
        /// `SIG_IGN` is 1 on Linux and macOS.
        const SIG_IGN: usize = 1;
        // SIGHUP, SIGINT and SIGTERM are 1, 2 and 15 on Linux and macOS.
        for sig in [1, 2, 15] {
            // SAFETY: `on_signal` is an `extern "C"` function that lives for
            // the whole process and only stores to a static atomic; SIG_IGN
            // is the documented ignore disposition.
            unsafe {
                let before = signal(sig, on_signal as extern "C" fn(c_int) as usize);
                // L3: an ignored SIGHUP (`nohup`) stays ignored. Between the
                // two calls a SIGHUP would set the flag: a window of two
                // system calls, accepted over sigaction's per-target layouts.
                if sig == 1 && before == SIG_IGN {
                    signal(sig, SIG_IGN);
                }
            }
        }
    }

    #[cfg(windows)]
    extern "system" fn on_ctrl(kind: u32) -> i32 {
        // CTRL_C_EVENT 0 and CTRL_BREAK_EVENT 1 are handled here; a closed
        // console or a logoff goes on to the default handler.
        if kind <= 1 {
            REQUESTED.store(true, Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    /// Installs the handler (once per process is enough; again is harmless).
    #[cfg(windows)]
    pub(crate) fn install() {
        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
        }
        // SAFETY: `on_ctrl` is an `extern "system"` function that lives for
        // the whole process and only stores to a static atomic.
        unsafe {
            SetConsoleCtrlHandler(Some(on_ctrl), 1);
        }
    }

    /// No handler on this platform: the process stops as it always did.
    #[cfg(not(any(unix, windows)))]
    pub(crate) fn install() {}

    #[cfg(all(test, unix))]
    mod tests {
        use std::os::raw::c_int;
        use std::process::{Command, Stdio};

        extern "C" {
            fn signal(sig: c_int, handler: usize) -> usize;
        }
        const SIG_IGN: usize = 1;

        /// Not a test: installs the handler, then exits 0 when SIGHUP is
        /// still ignored and 7 when the handler took it.
        #[test]
        #[ignore = "not a test: the SIGHUP disposition check runs in a child"]
        fn hup_helper() {
            if std::env::var_os("ATLAS_HUP_HELPER").is_none() {
                return;
            }
            super::install();
            // SAFETY: reads the disposition by setting SIG_IGN, which is
            // what the check wants anyway; the process exits next.
            let now = unsafe { signal(1, SIG_IGN) };
            std::process::exit(if now == SIG_IGN { 0 } else { 7 });
        }

        fn run(ignored: bool) -> Option<i32> {
            use std::os::unix::process::CommandExt;
            let mut c = Command::new(std::env::current_exe().unwrap());
            c.args(["--ignored", "--exact", "platform::interrupt::tests::hup_helper", "--quiet"])
                .env("ATLAS_HUP_HELPER", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            if ignored {
                // SAFETY: signal is async-signal-safe and takes integers.
                unsafe {
                    c.pre_exec(|| {
                        signal(1, SIG_IGN);
                        Ok(())
                    });
                }
            }
            c.status().unwrap().code()
        }

        /// L3: under `nohup` (SIGHUP ignored) the handler leaves SIGHUP
        /// ignored; without it, the handler takes SIGHUP.
        #[test]
        fn an_inherited_sighup_ignore_is_kept() {
            assert_eq!(run(true), Some(0), "SIGHUP was ignored before install and is not after");
            assert_eq!(run(false), Some(7), "the handler did not take SIGHUP");
        }
    }
}

/// Bytes from the operating system's random source, for secrets (the
/// webhook secret of `crate::pr_listen`). Owned FFI, no crate: Windows asks
/// `BCryptGenRandom` for the system-preferred generator, Linux and macOS
/// read `/dev/urandom`.
pub(crate) mod random {
    use std::io;

    /// Fills `buf` from the system's cryptographic random source.
    #[cfg(windows)]
    pub(crate) fn fill(buf: &mut [u8]) -> io::Result<()> {
        use std::ffi::c_void;

        #[link(name = "bcrypt")]
        extern "system" {
            fn BCryptGenRandom(alg: *mut c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
        }
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x2;
        for chunk in buf.chunks_mut(1 << 20) {
            let len = u32::try_from(chunk.len()).map_err(|_| io::Error::other("random: chunk too large"))?;
            // SAFETY: the pointer and length describe `chunk`, which the call
            // only writes within; a null algorithm handle with
            // BCRYPT_USE_SYSTEM_PREFERRED_RNG is the documented form.
            let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), chunk.as_mut_ptr(), len, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
            if status < 0 {
                return Err(io::Error::other(format!("BCryptGenRandom failed: NTSTATUS {status:#x}")));
            }
        }
        Ok(())
    }

    /// Fills `buf` from the system's cryptographic random source.
    #[cfg(unix)]
    pub(crate) fn fill(buf: &mut [u8]) -> io::Result<()> {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(buf)
    }

    /// No known random source on this platform.
    #[cfg(not(any(unix, windows)))]
    pub(crate) fn fill(_buf: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "no system random source on this platform"))
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn two_draws_differ_and_are_not_zero() {
            let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
            super::fill(&mut a).unwrap();
            super::fill(&mut b).unwrap();
            assert_ne!(a, b);
            assert_ne!(a, [0u8; 32]);
        }
    }
}
