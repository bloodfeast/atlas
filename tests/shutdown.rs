//! #221, ruling journal:ATLAS-RELEASE:0070 Q9: the console handler gets
//! the logoff and shutdown events only while the process has not loaded
//! user32 or gdi32. The shipped binary must not import either: the
//! end-session window loads user32 at run time, only where it is needed.

#[cfg(windows)]
#[test]
fn the_atlas_binary_imports_neither_user32_nor_gdi32() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_atlas")).unwrap();
    let dlls = imports(&exe);
    assert!(dlls.iter().any(|d| d.eq_ignore_ascii_case("kernel32.dll")), "the import table did not read: {dlls:?}");
    for bad in ["user32.dll", "gdi32.dll"] {
        assert!(!dlls.iter().any(|d| d.eq_ignore_ascii_case(bad)), "atlas imports {bad}: {dlls:?}");
    }
}

/// The DLL names a PE32+ image imports, directly or delay-loaded.
#[cfg(windows)]
fn imports(pe: &[u8]) -> Vec<String> {
    let u16_at = |o: usize| u16::from_le_bytes([pe[o], pe[o + 1]]) as usize;
    let u32_at = |o: usize| u32::from_le_bytes([pe[o], pe[o + 1], pe[o + 2], pe[o + 3]]) as usize;
    let nt = u32_at(0x3c);
    assert_eq!(&pe[nt..nt + 4], b"PE\0\0");
    let coff = nt + 4;
    let sections = u16_at(coff + 2);
    let opt = coff + 20;
    assert_eq!(u16_at(opt), 0x20b, "not PE32+");
    let dirs = opt + 112;
    let table = opt + u16_at(coff + 16);
    let file_of = |rva: usize| -> Option<usize> {
        (0..sections).map(|i| table + i * 40).find_map(|s| {
            let (va, size, raw) = (u32_at(s + 12), u32_at(s + 8).max(u32_at(s + 16)), u32_at(s + 20));
            (rva >= va && rva < va + size).then(|| rva - va + raw)
        })
    };
    let name_at = |rva: usize| -> String {
        let Some(o) = file_of(rva) else { return String::new() };
        let end = pe[o..].iter().position(|&b| b == 0).unwrap_or(0);
        String::from_utf8_lossy(&pe[o..o + end]).into_owned()
    };
    let mut out = Vec::new();
    // (directory index, descriptor size, offset of the name's RVA)
    for (dir, step, name) in [(1usize, 20usize, 12usize), (13, 32, 4)] {
        let rva = u32_at(dirs + dir * 8);
        let Some(mut at) = file_of(rva).filter(|_| rva != 0) else { continue };
        while pe[at..at + step].iter().any(|&b| b != 0) {
            out.push(name_at(u32_at(at + name)));
            at += step;
        }
    }
    out
}
