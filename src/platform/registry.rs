//! Read-only Windows registry lookups under `HKEY_LOCAL_MACHINE`, for the
//! scheduled-restart watch (#221): whether a key exists, and a value's
//! DWORD or QWORD. Owned advapi32 FFI, no crate. Nothing here writes.
//! Elsewhere every lookup is `Reg::Missing`.

pub(crate) use crate::restart_warn::Reg;

/// Looks `path` (under `HKEY_LOCAL_MACHINE`, the 64-bit view) up: with
/// `value` `None`, whether the key exists; else that value.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn hklm(path: &str, value: Option<&str>) -> Reg {
    sys::hklm(path, value)
}

#[cfg(windows)]
mod sys {
    use super::Reg;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(key: isize, sub: *const u16, options: u32, sam: u32, out: *mut isize) -> i32;
        fn RegQueryValueExW(key: isize, name: *const u16, reserved: *mut u32, kind: *mut u32, data: *mut u8, len: *mut u32) -> i32;
        fn RegCloseKey(key: isize) -> i32;
    }

    /// `HKEY_LOCAL_MACHINE`: `(HKEY)(LONG)0x80000002`, sign-extended.
    const HKLM: isize = 0x8000_0002_u32 as i32 as isize;
    /// `KEY_QUERY_VALUE | KEY_WOW64_64KEY`: read only, the 64-bit view.
    const SAM: u32 = 0x0001 | 0x0100;
    const REG_DWORD: u32 = 4;
    const REG_QWORD: u32 = 11;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn hklm(path: &str, value: Option<&str>) -> Reg {
        let p = wide(path);
        let mut key: isize = 0;
        // SAFETY: `p` is NUL-terminated and outlives the call; `key` is a
        // live local the call writes the opened handle to.
        if unsafe { RegOpenKeyExW(HKLM, p.as_ptr(), 0, SAM, &mut key) } != 0 {
            return Reg::Missing;
        }
        let Some(v) = value else {
            // SAFETY: `key` is the handle opened above, closed once.
            unsafe { RegCloseKey(key) };
            return Reg::Key;
        };
        let n = wide(v);
        let mut kind = 0u32;
        let mut data = [0u8; 8];
        let mut len = data.len() as u32;
        // SAFETY: `key` is open; `n` is NUL-terminated; `data` holds `len`
        // bytes and the call writes at most that many (a larger value fails
        // with ERROR_MORE_DATA and is reported as `Other`).
        let r = unsafe { RegQueryValueExW(key, n.as_ptr(), std::ptr::null_mut(), &mut kind, data.as_mut_ptr(), &mut len) };
        // SAFETY: as above, closed once.
        unsafe { RegCloseKey(key) };
        const ERROR_MORE_DATA: i32 = 234;
        match (r, kind, len) {
            (0, REG_DWORD, 4) => Reg::Dword(u32::from_le_bytes([data[0], data[1], data[2], data[3]])),
            (0, REG_QWORD, 8) => Reg::Qword(u64::from_le_bytes(data)),
            (0, _, _) | (ERROR_MORE_DATA, _, _) => Reg::Other,
            _ => Reg::Missing,
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::Reg;

    pub(super) fn hklm(_path: &str, _value: Option<&str>) -> Reg {
        Reg::Missing
    }
}
