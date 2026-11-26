//! `scripts/launcher.sh`, the plugin's `bin/atlas` on macOS and Linux.

#![cfg(unix)]

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::Command;

fn uname(flag: &str) -> String {
    String::from_utf8(Command::new("uname").arg(flag).output().unwrap().stdout).unwrap().trim().to_string()
}

/// The prebuilt binary's name the launcher looks for on this machine.
fn binary_name() -> String {
    let os = match uname("-s").as_str() {
        "Linux" => "unknown-linux-gnu",
        "Darwin" => "apple-darwin",
        _ => "unknown",
    };
    let arch = match uname("-m").as_str() {
        "x86_64" | "amd64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        _ => "unknown",
    };
    format!("atlas-{arch}-{os}")
}

fn executable(p: &Path, text: &str) {
    std::fs::write(p, text).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A plugin's bin/ folder: the launcher as `bin/atlas` beside a stand-in
/// binary that prints its arguments.
fn plugin(root: &Path) -> std::path::PathBuf {
    let bin = root.join("plugin/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let launcher = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/launcher.sh")).unwrap();
    executable(&bin.join("atlas"), &launcher);
    executable(&bin.join(binary_name()), "#!/bin/sh\necho \"the binary: $*\"\n");
    bin
}

fn run(p: &Path) -> String {
    let out = Command::new(p).arg("--version").output().unwrap();
    assert!(out.status.success(), "{}: {}", p.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn the_launcher_starts_the_binary_beside_it_even_through_links() {
    let root = std::env::temp_dir().join(format!("atlas-launcher-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bin = plugin(&root);
    assert_eq!(run(&bin.join("atlas")), "the binary: --version\n");

    // ~/.local/bin/atlas linked to the launcher, absolutely and relatively,
    // and a link to that link: the binary is beside the launcher, not the link.
    let local = root.join("local/bin");
    std::fs::create_dir_all(&local).unwrap();
    symlink(bin.join("atlas"), local.join("atlas")).unwrap();
    assert_eq!(run(&local.join("atlas")), "the binary: --version\n");
    symlink("../../plugin/bin/atlas", local.join("atlas-rel")).unwrap();
    assert_eq!(run(&local.join("atlas-rel")), "the binary: --version\n");
    symlink(local.join("atlas-rel"), root.join("atlas-chain")).unwrap();
    assert_eq!(run(&root.join("atlas-chain")), "the binary: --version\n");

    std::fs::remove_dir_all(&root).unwrap();
}
