//! When the machine last booted (#221): a restart point written before it
//! is from a machine that went down since. What each OS gives cheaply:
//! Windows `GetTickCount64` (kernel32, already linked), Linux `/proc/stat`'s
//! `btime`, macOS `sysctl kern.boottime`. `None` where it cannot be read.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The machine's boot time, or `None` where it cannot be read.
pub(crate) fn boot_time() -> Option<SystemTime> {
    sys::boot_time()
}

/// `/proc/stat`'s text: its `btime` line, seconds since the epoch.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn btime(stat: &str) -> Option<SystemTime> {
    stat.lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

#[cfg(windows)]
mod sys {
    use std::time::{Duration, SystemTime};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetTickCount64() -> u64;
    }

    pub(super) fn boot_time() -> Option<SystemTime> {
        // SAFETY: no arguments; it only reads the system's tick count.
        let up = unsafe { GetTickCount64() };
        SystemTime::now().checked_sub(Duration::from_millis(up))
    }
}

#[cfg(target_os = "linux")]
mod sys {
    use std::time::SystemTime;

    pub(super) fn boot_time() -> Option<SystemTime> {
        super::btime(&std::fs::read_to_string("/proc/stat").ok()?)
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use std::os::raw::{c_char, c_int, c_void};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// macOS's `struct timeval` (`time_t` 64-bit, `suseconds_t` 32-bit,
    /// padded to 16 bytes).
    #[repr(C)]
    struct TimeVal {
        sec: i64,
        usec: i32,
    }

    extern "C" {
        fn sysctlbyname(name: *const c_char, old: *mut c_void, oldlen: *mut usize, new: *mut c_void, newlen: usize) -> c_int;
    }

    pub(super) fn boot_time() -> Option<SystemTime> {
        let mut tv = TimeVal { sec: 0, usec: 0 };
        let mut len = std::mem::size_of::<TimeVal>();
        // SAFETY: the name is NUL-terminated; `tv` and `len` are live locals
        // and `len` is `tv`'s size, which the call writes no more than.
        let r = unsafe { sysctlbyname(c"kern.boottime".as_ptr(), (&raw mut tv).cast(), &raw mut len, std::ptr::null_mut(), 0) };
        if r != 0 || tv.sec <= 0 {
            return None;
        }
        let usec = u64::try_from(tv.usec).unwrap_or(0);
        Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(tv.sec).ok()?) + Duration::from_micros(usec))
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod sys {
    pub(super) fn boot_time() -> Option<std::time::SystemTime> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btime_is_read_from_proc_stat() {
        let stat = "cpu  1 2 3 4\nintr 5\nctxt 6\nbtime 1763343420\nprocesses 7\n";
        assert_eq!(btime(stat), Some(UNIX_EPOCH + Duration::from_secs(1_763_343_420)));
        assert_eq!(btime("cpu 1\n"), None);
    }

    /// This machine's boot time is read, and lies in the past.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_machine_s_boot_time_is_in_the_past() {
        let b = boot_time().expect("boot time");
        assert!(b < SystemTime::now(), "{b:?}");
    }
}
