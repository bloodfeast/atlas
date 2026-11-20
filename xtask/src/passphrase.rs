//! The signing key's passphrase, kept for an unattended release (Brandon
//! 2026-11-19: "Fully unattended, local"). The owner stores it once with
//! `cargo xtask release store-passphrase`, at a terminal, typed into a
//! prompt that does not echo; it is encrypted with Windows DPAPI under his
//! account on this machine (CryptProtectData, with an entropy string that
//! names this use) and written to the store file. `release ship` and
//! `release sign` decrypt it with no prompt and hand it to openssl on its
//! standard input (`-passin stdin`), never on a command line, in an
//! environment variable or in any output. Nothing here prints it, and its
//! bytes are zeroed when dropped.
//!
//! The store file: ATLAS_SIGN_PASSPHRASE_STORE, else
//! `%LOCALAPPDATA%/atlas-release/sign-passphrase.dpapi`. DPAPI exists on
//! Windows only; elsewhere the store is never used and openssl asks for the
//! passphrase at the terminal as before.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

type R<T = ()> = Result<T, String>;

/// DPAPI's optional entropy: a blob made for another use does not decrypt
/// as this one.
#[cfg(windows)]
const ENTROPY: &[u8] = b"atlas-release-sign-passphrase-v1";

/// The passphrase, zeroed on drop and never shown by `Debug`.
pub struct Secret(Vec<u8>);

impl Secret {
    /// Its bytes, for openssl's standard input only.
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            // A volatile write, so the zeroing is not optimised away.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<passphrase>)")
    }
}

/// The store file's path (see the module docs).
pub fn store_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ATLAS_SIGN_PASSPHRASE_STORE").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("LOCALAPPDATA").filter(|p| !p.is_empty()).map(PathBuf::from).unwrap_or_else(|| crate::release::home().join(".atlas-release"));
    base.join("atlas-release").join("sign-passphrase.dpapi")
}

/// The store this process signs with, once armed by ship or sign.
static ARMED: OnceLock<PathBuf> = OnceLock::new();

/// Makes `sign_manifest` take the passphrase from `store` instead of the
/// terminal. Tests never arm it, so their keys are read as before.
pub fn arm(store: &Path) {
    let _ = ARMED.set(store.to_path_buf());
}

/// The armed store, if any.
pub fn armed() -> Option<&'static Path> {
    ARMED.get().map(PathBuf::as_path)
}

/// Whether this platform keeps the passphrase (DPAPI: Windows).
pub const SUPPORTED: bool = cfg!(windows);

/// The message when the store is missing: what the owner runs once.
pub fn missing(store: &Path) -> String {
    format!(
        "no stored signing passphrase at {}: the unattended release signs with it. The owner runs `cargo xtask release store-passphrase` once, at a terminal (it asks twice, without echo, and keeps it under his Windows account with DPAPI); then run ship again",
        store.display()
    )
}

/// Ok when the store file is there (it is not decrypted here).
pub fn require(store: &Path) -> R {
    if store.is_file() {
        Ok(())
    } else {
        Err(missing(store))
    }
}

/// The stored passphrase, decrypted (Windows only).
pub fn load(store: &Path) -> R<Secret> {
    let blob = std::fs::read(store).map_err(|_| missing(store))?;
    dpapi::unprotect(&blob).map(Secret).map_err(|e| format!("{}: cannot decrypt the stored passphrase ({e}); it opens only under the account and machine that stored it: run `cargo xtask release store-passphrase` again", store.display()))
}

/// Runs openssl `cmd` with the passphrase on its standard input (the
/// command must hold `-passin stdin`). Its output goes where `cmd` says.
pub fn run_with(cmd: &mut Command, pass: &Secret) -> R<bool> {
    let mut child = cmd.stdin(Stdio::piped()).spawn().map_err(|e| format!("openssl: {e}"))?;
    {
        let mut stdin = child.stdin.take().ok_or("openssl: no stdin")?;
        // A key that is not encrypted never reads it; a closed pipe then
        // is not an error.
        let _ = stdin.write_all(pass.bytes()).and_then(|_| stdin.write_all(b"\n"));
    }
    child.wait().map(|s| s.success()).map_err(|e| format!("openssl: {e}"))
}

/// Whether the passphrase opens the key (`openssl pkey -noout`): checked
/// before anything public happens, so a wrong one stops the run early.
pub fn opens(openssl: &Path, key: &Path, pass: &Secret) -> R<bool> {
    let mut c = Command::new(openssl);
    c.args(["pkey", "-noout", "-passin", "stdin", "-in"]).arg(key).stdout(Stdio::null()).stderr(Stdio::null());
    run_with(&mut c, pass)
}

/// `release store-passphrase`: asks twice at a terminal without echo,
/// checks the answer opens the key when one is configured, encrypts it with
/// DPAPI and writes the store. Prints nothing of it.
pub fn store_cmd(store: &Path, key: Option<&Path>) -> R {
    use std::io::IsTerminal;
    if !SUPPORTED {
        return Err("the stored passphrase needs Windows DPAPI; on this platform openssl asks for it when signing".into());
    }
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Err("store-passphrase needs the owner at a terminal: it reads the passphrase from a prompt that does not echo, never from a pipe or an argument".into());
    }
    let first = dpapi::read_hidden("signing key passphrase: ")?;
    let again = dpapi::read_hidden("again: ")?;
    if first.bytes() != again.bytes() {
        return Err("the two answers differ; nothing was stored".into());
    }
    if first.bytes().is_empty() {
        return Err("an empty passphrase; nothing was stored".into());
    }
    if let (Some(key), Some(openssl)) = (key, crate::release::which("openssl")) {
        if !opens(&openssl, key, &first)? {
            return Err(format!("that passphrase does not open the signing key {}; nothing was stored", key.display()));
        }
        println!("release: the passphrase opens the signing key {}", key.display());
    }
    let blob = dpapi::protect(first.bytes())?;
    crate::release::mkdir(store.parent().ok_or("the store has no folder")?)?;
    std::fs::write(store, blob).map_err(|e| format!("{}: {e}", store.display()))?;
    println!("release: stored under this Windows account with DPAPI at {}", store.display());
    Ok(())
}

#[cfg(windows)]
mod dpapi {
    use super::{Secret, ENTROPY};
    use std::ffi::c_void;
    use std::io::BufRead as _;
    use std::io::Write as _;

    #[repr(C)]
    struct Blob {
        cb: u32,
        pb: *mut u8,
    }

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const ENABLE_ECHO_INPUT: u32 = 0x4;

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(data_in: *const Blob, descr: *const u16, entropy: *const Blob, reserved: *mut c_void, prompt: *mut c_void, flags: u32, data_out: *mut Blob) -> i32;
        fn CryptUnprotectData(data_in: *const Blob, descr: *mut *mut u16, entropy: *const Blob, reserved: *mut c_void, prompt: *mut c_void, flags: u32, data_out: *mut Blob) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
        fn GetLastError() -> u32;
    }

    fn blob(b: &[u8]) -> Blob {
        Blob { cb: b.len() as u32, pb: b.as_ptr().cast_mut() }
    }

    /// Copies a blob DPAPI allocated, zeroes it and frees it.
    unsafe fn take(out: &Blob) -> Vec<u8> {
        let v = std::slice::from_raw_parts(out.pb, out.cb as usize).to_vec();
        for i in 0..out.cb as usize {
            std::ptr::write_volatile(out.pb.add(i), 0);
        }
        LocalFree(out.pb.cast());
        v
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        let (input, entropy) = (blob(plain), blob(ENTROPY));
        let mut out = Blob { cb: 0, pb: std::ptr::null_mut() };
        // SAFETY: every pointer is to a live local; DPAPI allocates `out`,
        // which `take` frees.
        let ok = unsafe { CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null_mut(), std::ptr::null_mut(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) };
        if ok == 0 {
            return Err(format!("CryptProtectData failed (error {})", unsafe { GetLastError() }));
        }
        Ok(unsafe { take(&out) })
    }

    pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, String> {
        let (input, entropy) = (blob(sealed), blob(ENTROPY));
        let mut out = Blob { cb: 0, pb: std::ptr::null_mut() };
        // SAFETY: as in `protect`.
        let ok = unsafe { CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null_mut(), std::ptr::null_mut(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) };
        if ok == 0 {
            return Err(format!("CryptUnprotectData failed (error {})", unsafe { GetLastError() }));
        }
        Ok(unsafe { take(&out) })
    }

    /// One line from the console with echo off, restored after.
    pub fn read_hidden(prompt: &str) -> Result<Secret, String> {
        print!("{prompt}");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        // SAFETY: the standard input handle and a local mode word.
        let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode = 0u32;
        if unsafe { GetConsoleMode(h, &mut mode) } == 0 {
            return Err("standard input is not a console".into());
        }
        if unsafe { SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT) } == 0 {
            return Err("cannot turn the console echo off".into());
        }
        // Pre-sized, so reading a passphrase of any usual length never
        // grows (and so copies and frees unzeroed) the buffer; the Secret
        // then owns that same buffer, trimmed in place, and zeroes it on
        // drop. The standard input's own read buffer is the one copy left.
        let mut line = Secret(Vec::with_capacity(1024));
        let read = std::io::stdin().lock().read_until(b'\n', &mut line.0);
        unsafe { SetConsoleMode(h, mode) };
        println!();
        read.map_err(|e| e.to_string())?;
        while line.0.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            let n = line.0.len() - 1;
            unsafe { std::ptr::write_volatile(&mut line.0[n], 0) };
            line.0.truncate(n);
        }
        Ok(line)
    }
}

#[cfg(not(windows))]
mod dpapi {
    use super::Secret;

    pub fn protect(_: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI is Windows only".into())
    }

    pub fn unprotect(_: &[u8]) -> Result<Vec<u8>, String> {
        Err("DPAPI is Windows only".into())
    }

    pub fn read_hidden(_: &str) -> Result<Secret, String> {
        Err("a hidden prompt is implemented for Windows only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_store_says_what_the_owner_runs_once() {
        let p = std::env::temp_dir().join(format!("atlas-pass-missing-{}", std::process::id())).join("none.dpapi");
        let e = require(&p).unwrap_err();
        assert!(e.contains("cargo xtask release store-passphrase") && e.contains("once"), "{e}");
        assert!(load(&p).is_err());
    }

    #[test]
    fn the_secret_never_shows_in_debug() {
        let s = Secret(b"hunter2-not-real".to_vec());
        assert_eq!(format!("{s:?}"), "Secret(<passphrase>)");
    }

    /// DPAPI round trip under this account, and a blob tied to another
    /// entropy (another use) does not decrypt.
    #[cfg(windows)]
    #[test]
    fn dpapi_round_trips_under_this_account() {
        let sealed = dpapi::protect(b"synthetic test passphrase").unwrap();
        assert!(!sealed.windows(9).any(|w| w == b"synthetic"), "the blob is not the plain text");
        assert_eq!(dpapi::unprotect(&sealed).unwrap(), b"synthetic test passphrase");
        let mut broken = sealed.clone();
        let n = broken.len();
        broken[n - 1] ^= 0xff;
        assert!(dpapi::unprotect(&broken).is_err());
        let d = std::env::temp_dir().join(format!("atlas-pass-rt-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("s.dpapi");
        std::fs::write(&f, &sealed).unwrap();
        assert_eq!(load(&f).unwrap().bytes(), b"synthetic test passphrase");
        let _ = std::fs::remove_dir_all(&d);
    }
}
