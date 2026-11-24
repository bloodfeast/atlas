//! A file's identity, for telling whether an open handle and a path name
//! the same file (issue #350: a log handle whose file a rotation replaced).
//! Unix: device and inode. Windows: the volume serial number and file index
//! of `GetFileInformationByHandle`. Elsewhere `None`: unknown.

use std::fs::File;

/// The identity of the open file `f`, or `None` where it cannot be read.
pub(crate) fn of(f: &File) -> Option<(u64, u64)> {
    sys::of(f)
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::os::unix::fs::MetadataExt;

    pub(super) fn of(f: &File) -> Option<(u64, u64)> {
        let m = f.metadata().ok()?;
        Some((m.dev(), m.ino()))
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    /// `BY_HANDLE_FILE_INFORMATION`.
    /// The call's layout: only the volume and index fields are read.
    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)]
    struct Info {
        attributes: u32,
        created: [u32; 2],
        accessed: [u32; 2],
        written: [u32; 2],
        volume_serial: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(h: *mut c_void, info: *mut Info) -> i32;
    }

    pub(super) fn of(f: &File) -> Option<(u64, u64)> {
        let mut info = Info::default();
        // SAFETY: the handle is `f`'s, open for the call's length; `info`
        // is a live local of the layout the call writes.
        let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) };
        (ok != 0).then(|| (u64::from(info.volume_serial), (u64::from(info.index_high) << 32) | u64::from(info.index_low)))
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use std::fs::File;

    pub(super) fn of(_f: &File) -> Option<(u64, u64)> {
        None
    }
}
