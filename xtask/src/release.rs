//! `cargo xtask release`: build atlas for release and make the public
//! release commit.
//!
//! The public repository receives releases only: one squashed commit per
//! version, made from the tracked tree of this repository (`git archive
//! HEAD`, so files marked export-ignore in .gitattributes stay out) plus the
//! prebuilt binaries in bin/, tagged vX.Y.Z, with the CHANGELOG.md section as
//! its notes. Nothing of this repository's history is pushed. Before the
//! commit is made, every exported file, binaries included, is checked against
//! the private denylist named by ATLAS_LEAK_DENYLIST (terms are never
//! printed), and after it, every author and committer address in the public
//! history must be the public address.
//!
//! ```text
//! cargo xtask release --build-only
//!     Build the binaries this machine can build into the stage folder's
//!     bin/, and stop. Run it on a Mac for the macOS binaries, then copy them
//!     into the publishing machine's prebuilt folder.
//! cargo xtask release --remote URL [--dry-run] [--allow-missing] [--skip-tests]
//!     The local rehearsal: checks, tests, builds, export, leak check, the
//!     commit and the tag in a clone of URL, the unsigned manifest and the
//!     wiki commit, all in the stage folder. It never pushes and never signs
//!     (--dry-run is accepted and changes nothing). The `--push` that
//!     published from here is gone: it skipped the pull request, the owner's
//!     approval and the public build, and needed the ruleset bypass. Every
//!     release goes through `cargo xtask release ship vX.Y.Z`.
//! ```
//!
//! The CI release (`--pr`, `assets`, `sign`, `ship`: a pull request to the
//! public repository, its build on GitHub's runners, the owner's signature
//! and publish) is in `release_ci.rs` and `release_ship.rs`; it shares the
//! checks, the export, the leak check, the notes and the manifest writer
//! ([`manifest_json`]) with this one, and the signing lives there.
//!
//! Targets and where they come from:
//!
//! * x86_64-pc-windows-msvc: built on Windows (bin/atlas.exe)
//! * x86_64-unknown-linux-gnu: built on Linux, or from Windows inside WSL
//! * aarch64-unknown-linux-gnu: built on an aarch64 Linux host, or on x86_64
//!   Linux (WSL included) with aarch64-linux-gnu-gcc and the rustup target
//!   installed
//! * aarch64-apple-darwin, x86_64-apple-darwin: built on macOS
//!   (--build-only), then copied in
//!
//! A target that cannot be built here is taken from the prebuilt folder
//! (ATLAS_PREBUILT, default <stage>/prebuilt) as atlas-<target>. A target
//! still missing stops the release unless --allow-missing is given; the
//! release notes then list it, and bin/atlas tells a user on that platform
//! how to build from source.
//!
//! Environment:
//!
//! * ATLAS_LEAK_DENYLIST: the private denylist (required except --build-only)
//! * ATLAS_RELEASE_STAGE: the stage folder (default
//!   ${CARGO_TARGET_DIR:-target}/release-stage)
//! * ATLAS_PREBUILT: binaries built elsewhere (default <stage>/prebuilt)
//! * ATLAS_MODEL_DIR: the model files, to verify against their pins and
//!   stage as release assets
//! * ATLAS_WSL_DISTRO: the WSL distribution for the Linux build on Windows
//!   (default: WSL's default)
//! * ATLAS_PUBLIC_REPO: owner/name of the public repository, for the notes,
//!   the wiki links and the guard (default [`DEFAULT_PUBLIC_REPO`])
//! * ATLAS_GITHUB_STUB: a folder of saved GitHub answers for the guard, in
//!   place of `gh api` (tests; see `public_repo.rs`)
//! * ATLAS_RELEASE_DATE: the date the notes carry (default today, UTC)
//!
//! The changelog rule: every release has a user-facing `## vX.Y.Z` section
//! in CHANGELOG.md with a `### Highlights` subsection holding text. A
//! missing or empty section, or one without highlights, stops the release
//! before anything is built.
//!
//! The guard: before any push, the public repository is checked on GitHub
//! (`public_repo.rs`): its name is not a redirect, it is public, it is not
//! the development repository, its owner is a user, only the admin role may
//! bypass its rulesets and only the owner is an admin. A push to GitHub
//! never happens here, so a failed check is reported.
//!
//! The wiki: `wiki/` of the exported tree, with the Changelog page
//! generated from CHANGELOG.md, is leak-checked and committed to
//! `<remote>.wiki.git` as one commit per release (`wiki.rs`); this run
//! prints what would change and pushes nothing; `--build-only` skips it. A wiki repository GitHub has not created yet stops the
//! release with the one-time step to create it.
//! * ATLAS_PUBLIC_NAME, ATLAS_PUBLIC_EMAIL: the public commit author
//! * CARGO_BUILD_JOBS: cargo's job count (default 4)
//!
//! Manifest: <stage>/release/ holds manifest.json (version, and per target
//! the file, its download URL, SHA-256 and size) and the binaries, unsigned.
//! `release sign` signs the CI release's manifest (`sign_manifest`, the raw
//! 64-byte ed25519 signature of openssl pkeyutl -sign -rawin); atlas checks
//! the signature against its built-in key before trusting anything in it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Usage.
pub const USAGE: &str = "usage: cargo xtask release --build-only
       cargo xtask release --remote URL [--dry-run] [--allow-missing] [--skip-tests]
       (publishing: cargo xtask release ship vX.Y.Z)";

pub(crate) const TARGETS: [&str; 5] =
    ["x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-apple-darwin"];

/// The toolchain the clippy gate runs on, here and in `.github/workflows/ci.yml`.
const CLIPPY_TOOLCHAIN: &str = "1.99.0";

/// The build script run inside WSL: the Linux binaries, from the same
/// archive, one after the other, on the Windows side's job count (`$2`;
/// WSL does not see CARGO_BUILD_JOBS), so the build keeps the machine's cap.
pub(crate) const WSL_BUILD: &str = r#"set -eu
stage="$1"
export CARGO_BUILD_JOBS="${2:-4}"
export PATH="$HOME/.cargo/bin:$PATH"
work="$HOME/.cache/atlas-release"
rm -rf "$work/src"
mkdir -p "$work/src"
tar -xf "$stage/src.tar" -C "$work/src"
cd "$work/src"
export CARGO_TARGET_DIR="$work/target" CARGO_PROFILE_RELEASE_STRIP=symbols ATLAS_RELEASE_BUILD=1
export CARGO_ENCODED_RUSTFLAGS="$(printf '%s\037%s\037%s' "--remap-path-prefix=$work/src=atlas" "--remap-path-prefix=$HOME/.cargo=cargo" "--remap-path-prefix=$(rustc --print sysroot)=rust")"
nice -n 10 cargo build --release --locked --target x86_64-unknown-linux-gnu -q
cp "$work/target/x86_64-unknown-linux-gnu/release/atlas" "$stage/bin/atlas-x86_64-unknown-linux-gnu"
# aarch64 by cross-compiling, when the cross gcc and the rustup target are installed.
if command -v aarch64-linux-gnu-gcc >/dev/null 2>&1 && rustup target list --installed | grep -qx aarch64-unknown-linux-gnu; then
    echo "release: build aarch64-unknown-linux-gnu (in WSL, cross)"
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
        nice -n 10 cargo build --release --locked --target aarch64-unknown-linux-gnu -q
    cp "$work/target/aarch64-unknown-linux-gnu/release/atlas" "$stage/bin/atlas-aarch64-unknown-linux-gnu"
else
    echo "release: aarch64-unknown-linux-gnu not built in WSL (needs aarch64-linux-gnu-gcc and the rustup target)"
fi
"#;

type R<T = ()> = Result<T, String>;

pub(crate) fn say(m: &str) {
    println!("release: {m}");
}

#[derive(Default)]
struct Opts {
    build_only: bool,
    allow_missing: bool,
    skip_tests: bool,
    remote: Option<String>,
}

pub(crate) fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// `command -v NAME`: the first match on PATH.
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|d| exts.iter().map(|e| d.join(format!("{name}{e}"))).find(|p| p.is_file()))
}

/// Runs a command; its stdout on success, else a message with its stderr.
pub(crate) fn output(c: &mut Command) -> R<String> {
    let o = c.stdin(Stdio::null()).output().map_err(|e| format!("{c:?}: {e}"))?;
    if !o.status.success() {
        return Err(format!("{c:?} failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Runs a command for its status, its output shown.
pub(crate) fn status(c: &mut Command) -> R<bool> {
    Ok(c.stdin(Stdio::null()).status().map_err(|e| format!("{c:?}: {e}"))?.success())
}

/// Files in a folder in the order `ls` gave them under en_US.UTF-8, which
/// release.sh ran under: letters and digits first, then punctuation, so
/// `atlas.exe` sorts between `atlas-aarch64-*` and `atlas-x86_64-*`.
pub(crate) fn collate_key(s: &str) -> (String, String) {
    (s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect(), s.to_string())
}

pub(crate) fn ls(dir: &Path) -> R<Vec<String>> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort_by_key(|s| collate_key(s));
    Ok(v)
}

pub(crate) fn copy(from: &Path, to: &Path) -> R {
    std::fs::copy(from, to).map(|_| ()).map_err(|e| format!("copy {} to {}: {e}", from.display(), to.display()))
}

pub(crate) fn sha256_file(p: &Path) -> R<String> {
    Ok(atlas::sha256::hex(&std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?))
}

pub(crate) fn size_of(p: &Path) -> R<u64> {
    Ok(std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?.len())
}

/// Removes `p` and everything under it; a missing path is fine. On Windows
/// a file that was just written (a fresh clone's `bin/atlas.exe`) can be
/// held for a moment by a virus scanner, so a sharing violation (os error
/// 32) or access denied (5) is retried for about five seconds first.
pub(crate) fn rm_rf(p: &Path) -> R {
    let mut tries = 0;
    loop {
        match std::fs::remove_dir_all(p) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if tries < 25 && matches!(e.raw_os_error(), Some(5 | 32)) => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => return Err(format!("remove {}: {e}", p.display())),
        }
    }
}

pub(crate) fn mkdir(p: &Path) -> R {
    std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))
}

pub(crate) fn write(p: &Path, b: impl AsRef<[u8]>) -> R {
    std::fs::write(p, b).map_err(|e| format!("{}: {e}", p.display()))
}

pub(crate) fn read_text(p: &Path) -> R<String> {
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

/// A path as the compiler sees it: on Windows, backslashes (what `cygpath
/// -w` gave the script).
fn native(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s
    }
}

pub(crate) fn bin_name(t: &str) -> String {
    if t.contains("-windows-") {
        "atlas.exe".into()
    } else {
        format!("atlas-{t}")
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Whether a file's first bytes are what its name says (see [`check_bin`]);
/// the bytes in hex when not.
pub(crate) fn magic_ok(f: &str, b: &[u8]) -> Result<(), String> {
    let magic = hex(&b[..b.len().min(4)]);
    let ok = (f == "atlas" && magic.starts_with("2321"))
        || (f.ends_with(".exe") && magic.starts_with("4d5a"))
        || (f.ends_with("-linux-gnu") && magic == "7f454c46")
        || (f.ends_with("-apple-darwin") && (magic == "cffaedfe" || magic == "cafebabe"));
    if ok {
        Ok(())
    } else {
        Err(magic)
    }
}

/// Each file in `dir` is what its name says: the launcher a script, a
/// Windows binary a PE image, the others ELF or Mach-O.
fn check_bin(dir: &Path) -> R {
    for f in ls(dir)? {
        let b = std::fs::read(dir.join(&f)).map_err(|e| format!("{}: {e}", dir.join(&f).display()))?;
        if let Err(magic) = magic_ok(&f, &b) {
            return Err(format!("{}/{f} is not the file its name says (starts {magic})", dir.display()));
        }
    }
    if !dir.join("atlas").is_file() {
        return Err(format!("{} has no launcher", dir.display()));
    }
    Ok(())
}

/// `sed -e '/./,$!d'`: drop the lines before the first non-empty one.
pub(crate) fn drop_leading_blank(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let first = lines.iter().position(|l| !l.is_empty()).unwrap_or(lines.len());
    lines[first..].join("\n")
}

/// A fence line's character, run length, indent and what follows the run:
/// up to three spaces, then three or more backticks or tildes (CommonMark;
/// four spaces or a tab make it an indented line, not a fence). The
/// fragment parser, scripts/changes-check.sh and the changelog readers
/// share this rule.
fn fence_run(l: &str) -> Option<(u8, usize, usize, &str)> {
    let t = l.trim_start_matches(' ');
    let k = l.len() - t.len();
    if k > 3 {
        return None;
    }
    let c = *t.as_bytes().first().filter(|c| **c == b'`' || **c == b'~')?;
    let n = t.bytes().take_while(|b| *b == c).count();
    (n >= 3).then(|| (c, n, k, &t[n..]))
}

/// A line's indent in columns: a space is one, a tab moves to the next
/// multiple of four (scripts/changes-check.sh counts the same way).
fn indent(l: &str) -> usize {
    let mut col = 0;
    for b in l.bytes() {
        match b {
            b' ' => col += 1,
            b'\t' => col += 4 - col % 4,
            _ => break,
        }
    }
    col
}

/// The first C0 control character in `text` other than a tab, a line feed or
/// a carriage return directly before a line feed: GitHub reads a lone
/// carriage return as a line break, which can start a comment, a heading or
/// a fence no line rule saw, and the rest can garble or hide text. The
/// fragment parser, scripts/changes-check.sh and the release share this rule.
pub(crate) fn stray_control(text: &str) -> Option<char> {
    let b = text.as_bytes();
    let bad = |(i, c): (usize, &u8)| *c < 0x20 && !matches!(c, b'\t' | b'\n') && !(*c == b'\r' && b.get(i + 1) == Some(&b'\n'));
    b.iter().enumerate().find(|p| bad(*p)).map(|(_, c)| char::from(*c))
}

/// What a refusal of [`stray_control`]'s character says.
pub(crate) fn control_why(c: char) -> String {
    format!(
        "a control character (U+{:04X}) other than a tab or a line ending, which GitHub can read as a line break or hide text with: remove it",
        u32::from(c)
    )
}

/// A list item's or a block quote's marker at the start of `s` (`-`, `*`,
/// `+`, `1.` or `1)` before a space, a tab or the line's end; `>`): what
/// follows it.
fn container_marker(s: &str) -> Option<&str> {
    if let Some(r) = s.strip_prefix('>') {
        return Some(r);
    }
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let r = if digits > 0 { s[digits..].strip_prefix(['.', ')']) } else { s.strip_prefix(['-', '*', '+']) }?;
    (r.is_empty() || r.starts_with([' ', '\t'])).then_some(r)
}

/// Whether a line (outside a fence) is a heading GitHub renders that does
/// not start at the line's first column: an ATX heading (one to six `#`,
/// then a space, a tab or the end) after spaces, tabs, or list or quote
/// markers (`- ## x`, `  # x`, `1. # x`, `> # x`). A `#` not followed so
/// (`#123`, `C#`) opens no heading.
pub(crate) fn nested_heading(l: &str) -> bool {
    let (mut s, mut nested) = (l, false);
    loop {
        let t = s.trim_start_matches([' ', '\t']);
        nested |= t.len() != s.len();
        s = t;
        match container_marker(s) {
            Some(r) => (s, nested) = (r, true),
            None => break,
        }
    }
    let hashes = s.bytes().take_while(|b| *b == b'#').count();
    nested && (1..=6).contains(&hashes) && matches!(s.as_bytes().get(hashes), None | Some(b' ' | b'\t'))
}

/// Whether a line (outside a fence) is a setext heading's underline: only
/// `=` or only `-`, after and before any spaces or tabs. Under a line of
/// text GitHub renders that text as a heading.
pub(crate) fn setext_underline(l: &str) -> bool {
    let t = l.trim_matches([' ', '\t']);
    !t.is_empty() && (t.bytes().all(|b| b == b'=') || t.bytes().all(|b| b == b'-'))
}

/// Fenced code blocks, read line by line: a block opens on a fence line (a
/// backtick fence whose info string holds no backtick) and closes on a
/// fence line of the same character, at least as long, with nothing after
/// it but spaces or tabs. Everything inside is opaque: a `#` line there is
/// no heading.
///
/// A block opened at indent k (a list item's) ends on GitHub at the first
/// line indented less than k, which ends the item: what follows is read
/// as Markdown again, and a later fence line opens a block that swallows
/// the rest. Such a line, the closing fence included, marks the reader
/// [`Fence::short`], which the parsers and the release refuse.
#[derive(Default)]
pub(crate) struct Fence {
    /// The open block's character, run length and indent.
    block: Option<(u8, usize, usize)>,
    /// A line inside a block was indented less than its opening fence.
    short: bool,
}

impl Fence {
    /// Takes the next line (no line ending): whether it belongs to a fenced
    /// block, its opening and closing lines included.
    pub(crate) fn opaque(&mut self, l: &str) -> bool {
        match self.block {
            Some((c, n, k)) => {
                if !l.trim_matches([' ', '\t']).is_empty() && indent(l) < k {
                    self.short = true;
                }
                if fence_run(l).is_some_and(|(c2, n2, _, rest)| c2 == c && n2 >= n && rest.trim_matches([' ', '\t']).is_empty()) {
                    self.block = None;
                }
                true
            }
            None => match fence_run(l) {
                Some((c, n, k, rest)) if !(c == b'`' && rest.contains('`')) => {
                    self.block = Some((c, n, k));
                    true
                }
                _ => false,
            },
        }
    }

    /// Whether a block is still open.
    pub(crate) fn open(&self) -> bool {
        self.block.is_some()
    }

    /// Whether a line inside a block was indented less than the fence that
    /// opened it (it stays set).
    pub(crate) fn short(&self) -> bool {
        self.short
    }
}

/// The `## vX.Y.Z` section of the changelog, without leading blank lines or
/// trailing newlines. A `## v` line inside a fenced block neither starts
/// nor ends a section.
pub(crate) fn changelog_section(text: &str, tag: &str) -> String {
    let want = format!("## {tag}");
    let mut on = false;
    let mut out = Vec::new();
    let mut fence = Fence::default();
    for l in text.lines() {
        let fenced = fence.opaque(l);
        if !on {
            on = !fenced && l == want;
            continue;
        }
        if !fenced && l.starts_with("## v") {
            break;
        }
        out.push(l);
    }
    let s = drop_leading_blank(&out.join("\n"));
    s.trim_end_matches('\n').to_string()
}

/// `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1`
pub(crate) fn cargo_version(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix("version = \"")?;
        let q = rest.rfind('"')?;
        Some(format!("{}{}", &rest[..q], &rest[q + 1..]))
    })
}

/// `sed -n 's/^ *"version": *"\(.*\)",*$/\1/p' plugin.json | head -1`
pub(crate) fn plugin_version(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let rest = l.trim_start_matches(' ').strip_prefix("\"version\":")?.trim_start_matches(' ').strip_prefix('"')?;
        let body = rest.trim_end_matches(',');
        Some(body.strip_suffix('"')?.to_string())
    })
}

/// A `pub const NAME: &str = "<64 hex>";` in src/update.rs.
pub(crate) fn key_const(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(&format!("pub const {name}: &str = \""))?;
        let (k, tail) = rest.split_at_checked(64)?;
        (k.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) && tail.starts_with("\";")).then(|| k.to_string())
    })
}

/// The model pins of config/atlas.template.json, as `(key, value)` pairs in
/// file order: lines `"file"`, `"sha256"`, `"size"` or `"id"` with a plain
/// value.
pub(crate) fn model_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start_matches(' ');
            let key = ["file", "sha256", "size", "id"].into_iter().find(|k| l.starts_with(&format!("\"{k}\":")))?;
            let v = l[key.len() + 3..].trim_start_matches(' ');
            let v = v.strip_prefix('"').unwrap_or(v);
            let end = v.find(['"', ',']).unwrap_or(v.len());
            let (val, tail) = v.split_at(end);
            let tail = tail.strip_prefix('"').unwrap_or(tail);
            let tail = tail.strip_prefix(',').unwrap_or(tail);
            tail.is_empty().then(|| (key.to_string(), val.to_string()))
        })
        .collect()
}

pub(crate) fn files_under(root: &Path, rel: &Path, out: &mut Vec<String>) -> R {
    for e in std::fs::read_dir(root.join(rel)).map_err(|e| format!("{}: {e}", root.join(rel).display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let r = rel.join(e.file_name());
        if e.file_type().map_err(|e| e.to_string())?.is_dir() {
            files_under(root, &r, out)?;
        } else {
            out.push(r.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// The leak check: every exported file's contents (binaries included) and
/// every path against every term, both normalised first (see
/// `leak.rs`, the matcher `tests/leak_check.rs` shares). Only a term's
/// number and the place are ever reported.
pub(crate) fn leak_check(tree: &Path, denylist: &Path) -> R<(usize, usize, Vec<String>)> {
    let mut files = Vec::new();
    files_under(tree, Path::new(""), &mut files)?;
    files.sort_by_key(|s| collate_key(s));
    let list = std::fs::read(denylist).map_err(|_| "the denylist is not readable".to_string())?;
    let list = String::from_utf8_lossy(&list).into_owned();
    let db = crate::ucd::Db::load(&crate::pack_embedder::default_ucd())?;
    let m = crate::leak::Matcher::new(&db, &list);
    let named: Vec<(String, Vec<u8>)> =
        files.into_iter().map(|f| std::fs::read(tree.join(&f)).map(|b| (f.clone(), b)).map_err(|e| format!("{f}: {e}"))).collect::<R<_>>()?;
    Ok((m.len(), named.len(), m.hits(&named)))
}

/// The public repository when ATLAS_PUBLIC_REPO is not set. The README's
/// wiki links name it (tests/docs.rs checks they agree).
pub const DEFAULT_PUBLIC_REPO: &str = "bloodfeast/atlas";

/// A changelog section split by its `### ` subsections: the text before
/// the first one (heading ""), then each `(heading, body)` in order, bodies
/// trimmed of blank lines at both ends.
fn subsections(section: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, Vec<&str>)> = vec![(String::new(), Vec::new())];
    let mut fence = Fence::default();
    for l in section.lines() {
        let fenced = fence.opaque(l);
        match l.strip_prefix("### ").filter(|_| !fenced) {
            Some(h) => out.push((h.trim().to_string(), Vec::new())),
            None => out.last_mut().expect("never empty").1.push(l),
        }
    }
    out.into_iter().map(|(h, b)| (h, b.join("\n").trim_matches('\n').to_string())).filter(|(h, b)| !h.is_empty() || !b.trim().is_empty()).collect()
}

/// Whether a block holds user-facing text: a line that is not blank, a
/// heading or an HTML comment.
fn has_text(s: &str) -> bool {
    s.lines().map(str::trim).any(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("<!--"))
}

/// The changelog rule (see the module docs): `Ok` when the section can be
/// released, else what is missing.
pub(crate) fn check_section(section: &str, tag: &str) -> R {
    if section.trim().is_empty() {
        return Err(format!("no {tag} section in CHANGELOG.md: every release gets a user-facing section (see the Releasing page)"));
    }
    if !has_text(section) {
        return Err(format!("the {tag} section of CHANGELOG.md is empty: write what changed for users before releasing"));
    }
    if let Some(c) = stray_control(section) {
        return Err(format!("the {tag} section of CHANGELOG.md has {}", control_why(c)));
    }
    let mut fence = Fence::default();
    for l in section.lines() {
        if fence.opaque(l) {
            continue;
        }
        // No exception for comments: a placeholder belongs outside the
        // released section.
        if l.trim_start_matches([' ', '\t']).starts_with('<') {
            return Err(format!(
                "the {tag} section of CHANGELOG.md has a line that starts with < outside a fenced block, which GitHub reads as HTML that can hide the rest of the notes (comments included): reword it, or put it in a fenced block"
            ));
        }
        if nested_heading(l) {
            return Err(format!(
                "the {tag} section of CHANGELOG.md has a heading after an indent or a list marker, which GitHub renders as a heading: reword it"
            ));
        }
        if setext_underline(l) {
            return Err(format!(
                "the {tag} section of CHANGELOG.md has a line of only = or -, which GitHub renders as a heading underline under the line above: reword it"
            ));
        }
    }
    if fence.open() {
        return Err(format!("the {tag} section of CHANGELOG.md has an unclosed fenced block, which swallows everything after it: close it"));
    }
    if fence.short() {
        return Err(format!(
            "the {tag} section of CHANGELOG.md has a line inside a fenced block indented less than the fence that opened it, which ends the block on GitHub: indent every line of the block, its closing fence included, at least as far as its opening fence"
        ));
    }
    let subs = subsections(section);
    if subs.iter().any(|(h, _)| !h.is_empty()) && !subs.iter().any(|(h, b)| h.eq_ignore_ascii_case("Highlights") && has_text(b)) {
        return Err(format!("the {tag} section of CHANGELOG.md has no ### Highlights with text: the release notes lead with them"));
    }
    Ok(())
}

/// The notes' three parts of a section: the highlights (the `Highlights`
/// subsection with any text before the first subsection; the whole section
/// when it has no subsections), the upgrade part (`Breaking changes` and
/// `Upgrade notes`, as `## ` sections) and the rest (`### ` sections).
fn notes_parts(section: &str) -> (String, String, String) {
    let subs = subsections(section);
    if subs.iter().all(|(h, _)| h.is_empty()) {
        return (section.trim_matches('\n').to_string(), String::new(), String::new());
    }
    let (mut hi, mut up, mut rest) = (Vec::new(), Vec::new(), Vec::new());
    for (h, b) in &subs {
        let lower = h.to_ascii_lowercase();
        if h.is_empty() || lower == "highlights" {
            hi.push(b.clone());
        } else if lower == "breaking changes" || lower == "upgrade notes" {
            up.push(format!("## {h}\n\n{b}"));
        } else {
            rest.push(format!("### {h}\n\n{b}"));
        }
    }
    (hi.join("\n\n"), up.join("\n\n"), rest.join("\n\n"))
}

/// Today's date, UTC, or ATLAS_RELEASE_DATE.
pub(crate) fn release_date() -> String {
    env("ATLAS_RELEASE_DATE").unwrap_or_else(|| atlas::discussion::utc_now()[..10].to_string())
}

/// What the notes are filled with.
pub(crate) struct Notes<'a> {
    pub(crate) tag: &'a str,
    pub(crate) date: &'a str,
    pub(crate) repo: &'a str,
    pub(crate) changes: &'a str,
    pub(crate) binaries: &'a str,
    pub(crate) missing: &'a str,
    pub(crate) models: &'a str,
}

/// The release notes from the template: whole-line placeholders replaced by
/// blocks (a block that comes out empty takes its line with it, and one
/// blank line after it), @VERSION@, @DATE@ and @REPO@ inline.
///
/// Blocks: @CHANGES@ (the whole section), @HIGHLIGHTS@, @UPGRADE@ and
/// @DETAILS@ (see [`notes_parts`]), @BINARIES@, @MISSING@, @MODELS@.
pub(crate) fn fill_notes(template: &str, n: &Notes) -> String {
    let (hi, up, rest) = notes_parts(n.changes);
    let rest = if rest.is_empty() { rest } else { format!("<details>\n<summary>Everything in {}</summary>\n\n{rest}\n\n</details>", n.tag) };
    let mut out = String::new();
    let mut skip_blank = false;
    for l in template.lines() {
        let block = match l {
            "@CHANGES@" => Some(n.changes.trim_matches('\n')),
            "@HIGHLIGHTS@" => Some(hi.as_str()),
            "@UPGRADE@" => Some(up.as_str()),
            "@DETAILS@" => Some(rest.as_str()),
            "@BINARIES@" => Some(n.binaries.trim_end_matches('\n')),
            "@MISSING@" => Some(n.missing.trim_matches('\n')),
            "@MODELS@" => Some(n.models.trim_end_matches('\n')),
            _ => None,
        };
        match block {
            Some("") => skip_blank = true,
            Some(b) => {
                skip_blank = false;
                for x in b.lines() {
                    out.push_str(x);
                    out.push('\n');
                }
            }
            None if skip_blank && l.is_empty() => skip_blank = false,
            None => {
                skip_blank = false;
                out.push_str(&l.replace("@VERSION@", n.tag).replace("@DATE@", n.date).replace("@REPO@", n.repo));
                out.push('\n');
            }
        }
    }
    out
}

pub(crate) fn platform_name(f: &str) -> &str {
    match f {
        "atlas.exe" => "Windows x86_64 (also runs on Windows on Arm)",
        "atlas-x86_64-unknown-linux-gnu" => "Linux x86_64 (glibc)",
        "atlas-aarch64-unknown-linux-gnu" => "Linux aarch64 (glibc)",
        "atlas-aarch64-apple-darwin" => "macOS Apple Silicon",
        "atlas-x86_64-apple-darwin" => "macOS Intel",
        f => f,
    }
}

/// The signing key's preflight, after every other check of a publish:
/// openssl is on PATH and the key's path names a file. The file is not
/// opened here; openssl alone opens it, when signing.
pub(crate) fn signing_ready(key: Option<&str>, openssl: Option<&Path>) -> R {
    if openssl.is_none() {
        return Err("publishing signs the manifest and needs openssl on PATH".into());
    }
    let key = key.ok_or("publishing needs ATLAS_SIGN_KEY (the release signing key)")?;
    let p = Path::new(key);
    if !p.is_file() {
        return Err("ATLAS_SIGN_KEY is not a file".into());
    }
    Ok(())
}

/// Signs the manifest with openssl: writes `<manifest>.sig`, the raw 64-byte
/// ed25519 signature, after checking the key's public half is the one built
/// into atlas, and verifies it. Only the public half is ever derived.
pub(crate) fn sign_manifest(key: &Path, trusted: &str, manifest: &Path) -> R {
    let openssl = which("openssl").ok_or("signing needs openssl")?;
    if !key.is_file() {
        return Err("ATLAS_SIGN_KEY is not a file".into());
    }
    let pub_der = PathBuf::from(format!("{}.pub.der", manifest.display()));
    let sig = PathBuf::from(format!("{}.sig", manifest.display()));
    let cleanup = |e: String| {
        let _ = std::fs::remove_file(&pub_der);
        e
    };
    // openssl asks for the passphrase on the terminal: stdin is inherited.
    let ok = Command::new(&openssl).args(["pkey", "-pubout", "-outform", "DER", "-in"]).arg(key).arg("-out").arg(&pub_der).status();
    if !ok.map(|s| s.success()).unwrap_or(false) {
        return Err(cleanup("cannot read the signing key".into()));
    }
    let der = std::fs::read(&pub_der).map_err(|e| cleanup(e.to_string()))?;
    let key_hex = hex(&der[der.len().saturating_sub(32)..]);
    if key_hex != trusted {
        return Err(cleanup(format!("ATLAS_SIGN_KEY is not the key built into atlas (its public half is {key_hex})")));
    }
    let ok = Command::new(&openssl)
        .args(["pkeyutl", "-sign", "-rawin", "-inkey"])
        .arg(key)
        .arg("-in")
        .arg(manifest)
        .arg("-out")
        .arg(&sig)
        .status();
    if !ok.map(|s| s.success()).unwrap_or(false) {
        return Err(cleanup("signing failed".into()));
    }
    if size_of(&sig).map_err(cleanup)? != 64 {
        return Err(cleanup("the signature is not 64 bytes".into()));
    }
    let ok = Command::new(&openssl)
        .args(["pkeyutl", "-verify", "-rawin", "-pubin", "-keyform", "DER", "-inkey"])
        .arg(&pub_der)
        .arg("-in")
        .arg(manifest)
        .arg("-sigfile")
        .arg(&sig)
        .stdout(Stdio::null())
        .status();
    if !ok.map(|s| s.success()).unwrap_or(false) {
        return Err(cleanup("the signature does not verify".into()));
    }
    let _ = std::fs::remove_file(&pub_der);
    Ok(())
}

pub(crate) struct Ctx {
    pub(crate) stage: PathBuf,
    pub(crate) cargo_home: PathBuf,
    pub(crate) sysroot: String,
}

impl Ctx {
    /// CARGO_ENCODED_RUSTFLAGS: paths compiled into the binaries (panic
    /// locations) are rewritten, so no build machine's folders reach a user.
    /// The sysroot too: with rust-src installed, std's panic locations name
    /// the local toolchain folder instead of /rustc/<hash>.
    pub(crate) fn rustflags(&self, target: &str) -> String {
        let mut f = [
            format!("--remap-path-prefix={}=atlas", native(&self.stage.join("src"))),
            format!("--remap-path-prefix={}=cargo", native(&self.cargo_home)),
            format!("--remap-path-prefix={}=rust", self.sysroot),
        ]
        .join("\x1f");
        if target.ends_with("-windows-msvc") {
            f.push_str("\x1f-Clink-arg=/PDBALTPATH:%_PDB%");
        }
        f
    }

    /// Builds `target` from the exported tree, not the working tree.
    fn build(&self, target: &str, extra_env: &[(&str, &str)]) -> R {
        say(&format!("build {target}"));
        let mut c = Command::new("cargo");
        c.current_dir(self.stage.join("src"))
            .env("CARGO_TARGET_DIR", self.stage.join("target"))
            .env("CARGO_PROFILE_RELEASE_STRIP", "symbols")
            .env("ATLAS_RELEASE_BUILD", "1")
            .env("CARGO_ENCODED_RUSTFLAGS", self.rustflags(target))
            .args(["build", "--release", "--locked", "--target", target, "-q"]);
        for (k, v) in extra_env {
            c.env(k, v);
        }
        if !status(&mut c)? {
            return Err(format!("build {target} failed"));
        }
        let ext = if target.contains("-windows-") { ".exe" } else { "" };
        copy(
            &self.stage.join("target").join(target).join("release").join(format!("atlas{ext}")),
            &self.stage.join("bin").join(bin_name(target)),
        )
    }

    /// The Linux binaries from Windows, built inside WSL from the same archive:
    /// x86_64 always, aarch64 when WSL has the cross gcc and the target.
    fn build_wsl(&self) -> R {
        say("build x86_64-unknown-linux-gnu (in WSL)");
        write(&self.stage.join("wsl-build.sh"), WSL_BUILD)?;
        let wsl = |args: &[&str]| {
            let mut c = Command::new("wsl.exe");
            if let Some(d) = env("ATLAS_WSL_DISTRO") {
                c.args(["-d", &d]);
            }
            c.arg("--").args(args);
            c
        };
        let win = self.stage.to_string_lossy().replace('\\', "/");
        let wsl_stage = output(&mut wsl(&["wslpath", "-a", &win]))?.trim_end_matches(['\r', '\n']).to_string();
        let jobs = env("CARGO_BUILD_JOBS").unwrap_or_else(|| "4".into());
        if !status(&mut wsl(&["sh", &format!("{wsl_stage}/wsl-build.sh"), &wsl_stage, &jobs]))? {
            return Err("the WSL build failed".into());
        }
        Ok(())
    }

    fn has_target(&self, t: &str) -> bool {
        output(Command::new("rustup").args(["target", "list", "--installed"]))
            .is_ok_and(|o| o.lines().any(|l| l.trim_end_matches('\r') == t))
    }

    /// Every target this host can build.
    pub(crate) fn build_here(&self) -> R {
        if cfg!(windows) {
            self.build("x86_64-pc-windows-msvc", &[])?;
            if which("wsl.exe").is_some() {
                self.build_wsl()?;
            }
        } else if cfg!(target_os = "linux") {
            match std::env::consts::ARCH {
                "x86_64" => {
                    self.build("x86_64-unknown-linux-gnu", &[])?;
                    if self.has_target("aarch64-unknown-linux-gnu") && which("aarch64-linux-gnu-gcc").is_some() {
                        self.build(
                            "aarch64-unknown-linux-gnu",
                            &[
                                ("CC_aarch64_unknown_linux_gnu", "aarch64-linux-gnu-gcc"),
                                ("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER", "aarch64-linux-gnu-gcc"),
                            ],
                        )?;
                    }
                }
                "aarch64" => self.build("aarch64-unknown-linux-gnu", &[])?,
                _ => {}
            }
        } else if cfg!(target_os = "macos") {
            for t in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
                if self.has_target(t) {
                    self.build(t, &[])?;
                } else {
                    say(&format!("skip {t}: rustup target add {t}"));
                }
            }
        } else {
            say(&format!("unknown host {}: nothing built here", std::env::consts::OS));
        }
        Ok(())
    }
}

fn parse(args: Vec<String>) -> R<Opts> {
    let mut o = Opts::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--build-only" => o.build_only = true,
            "--push" => return Err("--push is gone: a release goes through `cargo xtask release ship vX.Y.Z` (wiki/Releasing.md)".into()),
            // The local run never pushes or signs: --dry-run changes nothing.
            "--dry-run" => {}
            "--allow-missing" => o.allow_missing = true,
            "--skip-tests" => o.skip_tests = true,
            "--remote" => o.remote = Some(it.next().ok_or("--remote needs a URL")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument {a}")),
        }
    }
    Ok(o)
}

/// The repository root: the folder above this crate.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask sits in the repository").to_path_buf()
}

/// Runs the release.
pub fn run(args: Vec<String>) -> R {
    // The CI modes (`--pr`, `assets`, `sign`) live in release_ci.rs.
    if crate::release_ci::wants(&args) {
        return crate::release_ci::run(args);
    }
    let o = parse(args)?;
    let root = repo_root();
    // As the script's `cd`: a relative --remote or stage folder is read from the root.
    std::env::set_current_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let version = cargo_version(&read_text(&root.join("Cargo.toml"))?).ok_or("no version in Cargo.toml")?;
    let tag = format!("v{version}");
    let pv = plugin_version(&read_text(&root.join(".claude-plugin/plugin.json"))?).unwrap_or_default();
    if pv != version {
        return Err(format!("plugin.json version {pv} is not Cargo.toml's {version}"));
    }
    let jobs = env("CARGO_BUILD_JOBS").unwrap_or_else(|| "4".into());
    std::env::set_var("CARGO_BUILD_JOBS", &jobs);
    let stage_root = match env("ATLAS_RELEASE_STAGE") {
        Some(s) => root.join(s),
        None => root.join(env("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into())).join("release-stage"),
    };
    let stage = stage_root.join(&tag);
    let prebuilt = env("ATLAS_PREBUILT").map(|p| root.join(p)).unwrap_or_else(|| stage.join("prebuilt"));
    let public_repo = env("ATLAS_PUBLIC_REPO").unwrap_or_else(|| DEFAULT_PUBLIC_REPO.into());
    let public_name = env("ATLAS_PUBLIC_NAME").unwrap_or_else(|| "Brandon White".into());
    let public_email = env("ATLAS_PUBLIC_EMAIL").unwrap_or_else(|| "19593371+bloodfeast@users.noreply.github.com".into());
    let git = |args: &[&str]| output(Command::new("git").arg("-C").arg(&root).args(args));

    let (mut remote, mut denylist, mut changes) = (String::new(), PathBuf::new(), String::new());
    if !o.build_only {
        if !git(&["status", "--porcelain", "--untracked-files=no"])?.trim().is_empty() {
            return Err("uncommitted changes; the release is made from HEAD".into());
        }
        remote = o.remote.clone().ok_or("--remote URL is required (a local bare repository, or the public repository to clone)")?;
        denylist = PathBuf::from(env("ATLAS_LEAK_DENYLIST").ok_or("ATLAS_LEAK_DENYLIST is not set")?);
        if std::fs::File::open(&denylist).is_err() {
            return Err("the denylist is not readable".into());
        }
        crate::version::refuse_pending(&root)?;
        changes = changelog_section(&read_text(&root.join("CHANGELOG.md"))?, &tag);
        check_section(&changes, &tag)?;
        // The README, the wiki and the public files link the public
        // repository by name: they must name the one this release publishes to.
        let named = crate::public_repo::link_mismatches(&root, &public_repo)?;
        if !named.is_empty() {
            return Err(format!("ATLAS_PUBLIC_REPO is {public_repo}, but {}: change the links or ATLAS_PUBLIC_REPO", named.join("; ")));
        }
        // The guard, before anything is built: reported, as nothing is pushed.
        let dev = git(&["remote", "get-url", "origin"]).ok().map(|u| u.trim().to_string());
        let wiki = crate::wiki::wiki_remote(&remote);
        match crate::public_repo::check(crate::public_repo::api().as_ref(), &public_repo, dev.as_deref(), &[&remote, &wiki]) {
            Ok(r) => say(&format!("public repo check: {public_repo} is public, user-owned, {} ruleset(s) with admin-only bypass (main and v* tags as required), no admin among {} collaborator(s) but the owner", r.rulesets, r.collaborators)),
            Err(e) => say(&format!("public repo check: {e} (a publish would stop here)")),
        }
    }

    for d in ["src", "tree", "public", "bin", "assets"] {
        rm_rf(&stage.join(d))?;
    }
    mkdir(&stage.join("bin"))?;
    mkdir(&prebuilt)?;
    copy(&root.join("scripts/launcher.sh"), &stage.join("bin/atlas"))?;
    let src_tar = stage.join("src.tar");
    let ok = status(Command::new("git").arg("-C").arg(&root).args(["archive", "--format=tar", "-o"]).arg(&src_tar).arg("HEAD"))?;
    if !ok {
        return Err("git archive failed".into());
    }
    let archive = std::fs::read(&src_tar).map_err(|e| format!("{}: {e}", src_tar.display()))?;
    crate::tar::extract(&archive, &stage.join("src"))?;

    if !o.build_only && !o.skip_tests {
        // Clippy before the tests: it fails sooner. On a pinned toolchain, so a new
        // Rust release's lints cannot fail an unrelated release.
        for args in [&["--all-targets", "--features", "atlas/dev"][..], &[]] {
            say(&format!("clippy (cargo +{CLIPPY_TOOLCHAIN} clippy --workspace --locked {} -- -D warnings)", args.join(" ")));
            let ok = status(
                Command::new("cargo")
                    .current_dir(&root)
                    .env("CARGO_TARGET_DIR", stage.join("target"))
                    .arg(format!("+{CLIPPY_TOOLCHAIN}"))
                    .args(["clippy", "--workspace", "--locked"])
                    .args(args)
                    .args(["--", "-D", "warnings"]),
            )?;
            if !ok {
                return Err(format!("clippy failed, on a warning above or a missing toolchain: `rustup toolchain install {CLIPPY_TOOLCHAIN} --profile minimal -c clippy`"));
            }
        }
        // With the dev feature, so the measuring modes' tests run too; the
        // binaries are built without it below.
        say("tests (cargo test --release --locked --features dev, denylist set)");
        let out = Command::new("cargo")
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", stage.join("target"))
            .args(["test", "--release", "--locked", "--features", "dev", "--no-fail-fast", "-q"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cargo test: {e}"))?;
        let mut log = out.stdout.clone();
        log.extend_from_slice(&out.stderr);
        write(&stage.join("test.out"), &log)?;
        let log = String::from_utf8_lossy(&log);
        if !out.status.success() {
            let lines: Vec<&str> = log.lines().collect();
            for l in &lines[lines.len().saturating_sub(40)..] {
                println!("{l}");
            }
            return Err(format!("tests failed (whole log: {})", stage.join("test.out").display()));
        }
        let (p, f) = crate::verify_macos::test_totals(&log);
        say(&format!("tests passed {p}, failed {f}"));
    }

    let ctx = Ctx {
        stage: stage.clone(),
        cargo_home: env("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".cargo")),
        sysroot: output(Command::new("rustc").args(["--print", "sysroot"]))?.trim_end_matches(['\r', '\n']).to_string(),
    };
    ctx.build_here()?;

    if o.build_only {
        say(&format!("built: {}", ls(&stage.join("bin"))?.iter().map(|f| format!("{f} ")).collect::<String>()));
        say(&format!("copy them into the publishing machine's prebuilt folder ({})", prebuilt.display()));
        return Ok(());
    }

    let mut missing = String::new();
    for t in TARGETS {
        let f = bin_name(t);
        if stage.join("bin").join(&f).is_file() {
            continue;
        }
        if prebuilt.join(&f).is_file() {
            copy(&prebuilt.join(&f), &stage.join("bin").join(&f))?;
            say(&format!("prebuilt {t} taken from {}", prebuilt.display()));
        } else {
            missing.push(' ');
            missing.push_str(t);
        }
    }
    if !missing.is_empty() {
        if !o.allow_missing {
            return Err(format!("no binary for:{missing} (build them elsewhere into {}, or --allow-missing)", prebuilt.display()));
        }
        say(&format!("released without:{missing}"));
    }
    check_bin(&stage.join("bin"))?;

    say("export");
    let tree = stage.join("tree");
    crate::tar::extract(&archive, &tree)?;
    check_public_files(&tree)?;
    mkdir(&tree.join("bin"))?;
    for f in ls(&stage.join("bin"))? {
        copy(&stage.join("bin").join(&f), &tree.join("bin").join(&f))?;
    }
    check_bin(&tree.join("bin"))?;

    say("leak check (every exported file, binaries included)");
    let (n, nfiles, leaks) = leak_check(&tree, &denylist)?;
    write(&stage.join("leaks.txt"), leaks.iter().map(|l| format!("{l}\n")).collect::<String>())?;
    if n == 0 {
        return Err("the denylist holds no terms".into());
    }
    if !leaks.is_empty() {
        for l in &leaks {
            eprintln!("release: leak: {l}");
        }
        return Err(format!("{} leak(s) in the exported tree ({})", leaks.len(), tree.display()));
    }
    say(&format!("leak check: {n} terms, {nfiles} files, no hits"));
    crate::release_ci::secret_gate(&tree.join("bin"), "the release binaries")?;

    say("notes");
    let binaries = binaries_table(&stage.join("bin"))?;
    let missing_text = if missing.is_empty() {
        String::new()
    } else {
        format!("Not built for this release:{missing}. On those platforms, build from source (`cargo build --release --locked`); bin/atlas prints the steps.")
    };
    mkdir(&stage.join("assets"))?;
    let model_dir = env("ATLAS_MODEL_DIR").map(PathBuf::from);
    let mut models = String::new();
    for m in model_rows(&read_text(&root.join("config/atlas.template.json"))?) {
        let (mfile, msha, msize) = (&m.file, &m.sha256, &m.size);
        models.push_str(&m.row());
        if let Some(dir) = &model_dir {
            let src = dir.join(mfile);
            if !src.is_file() {
                return Err(format!("model {mfile} is not in ATLAS_MODEL_DIR"));
            }
            if size_of(&src)?.to_string() != *msize {
                return Err(format!("model {mfile}: size does not match its pin"));
            }
            if sha256_file(&src)? != *msha {
                return Err(format!("model {mfile}: sha256 does not match its pin"));
            }
            copy(&src, &stage.join("assets").join(mfile))?;
            say(&format!("model {mfile} verified against its pin and staged"));
        }
    }
    if model_dir.is_none() {
        say("ATLAS_MODEL_DIR not set: model assets not staged (they are a separate, version-independent release)");
    }
    let notes_path = stage.join("notes.md");
    let template = read_text(&root.join("scripts/release-notes.md.in"))?;
    let date = release_date();
    let notes = fill_notes(
        &template,
        &Notes { tag: &tag, date: &date, repo: &public_repo, changes: &changes, binaries: &binaries, missing: &missing_text, models: &models },
    );
    write(&notes_path, &notes)?;

    say("manifest");
    let update_rs = read_text(&root.join("src/update.rs"))?;
    let (trusted, test_key) = match (key_const(&update_rs, "TRUSTED_KEY_HEX"), key_const(&update_rs, "TEST_KEY_HEX")) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err("cannot read TRUSTED_KEY_HEX and TEST_KEY_HEX from src/update.rs".into()),
    };
    if trusted == test_key {
        say("warning: the key built into atlas is the test key");
    }
    let release_dir = stage.join("release");
    rm_rf(&release_dir)?;
    mkdir(&release_dir)?;
    for f in ls(&stage.join("bin"))? {
        if f != "atlas" {
            copy(&stage.join("bin").join(&f), &release_dir.join(&f))?;
        }
    }
    let manifest = release_dir.join("manifest.json");
    write(&manifest, manifest_json(&version, &public_repo, &stage.join("bin"), &manifest_notes(&root, &version, &changes)?)?)?;
    say("manifest not signed: this run never signs (ATLAS_SIGN_KEY is not read); `release ship` signs the release it publishes");

    say("commit");
    let public = stage.join("public");
    let ok = Command::new("git").args(["clone", "-q", &remote]).arg(&public).stdin(Stdio::null()).stderr(Stdio::null()).status();
    if !ok.map(|s| s.success()).unwrap_or(false) {
        return Err(format!("cannot clone {remote}"));
    }
    let pubc = |args: &[&str]| {
        let mut c = Command::new("git");
        c.arg("-C").arg(&public).args(args);
        c
    };
    let tree_arg = format!("--work-tree={}", tree.display());
    let pubt = |args: &[&str]| {
        let mut c = pubc(&[&tree_arg]);
        c.args(args);
        c
    };
    if !output(&mut pubc(&["ls-remote", "--tags", "origin", &format!("refs/tags/{tag}")]))?.trim().is_empty() {
        return Err(format!("{tag} already exists in {remote}"));
    }
    if status(pubc(&["rev-parse", "-q", "--verify", "HEAD"]).stdout(Stdio::null()))? {
        output(&mut pubc(&["checkout", "-q", "-B", "main"]))?;
        output(&mut pubc(&["rm", "-rq", "--ignore-unmatch", "."]))?;
    } else {
        output(&mut pubc(&["symbolic-ref", "HEAD", "refs/heads/main"]))?;
    }
    // Staged straight from the exported tree. bin/ is ignored in the source
    // tree (a local `cargo install --root .`); here it is the release.
    output(&mut pubt(&["add", "-A"]))?;
    output(&mut pubt(&["add", "-f", "bin"]))?;
    for f in ls(&tree.join("bin"))? {
        let staged = output(&mut pubc(&["rev-parse", &format!(":bin/{f}")]))?;
        let exported = output(Command::new("git").current_dir(&root).arg("hash-object").arg(tree.join("bin").join(&f)))?;
        if staged.trim() != exported.trim() {
            return Err(format!("bin/{f} in the commit is not the exported file"));
        }
    }
    for f in ls(&stage.join("bin"))? {
        if !f.ends_with(".exe") {
            output(&mut pubt(&["update-index", "--chmod=+x", &format!("bin/{f}")]))?;
        }
    }
    // The message: "atlas vX.Y.Z", then the notes without their title, kept
    // verbatim (the notes' Markdown headings start with #).
    let body = notes.split_once('\n').map_or("", |(_, b)| b);
    let message = stage.join("message.txt");
    write(&message, format!("atlas {tag}\n\n{}", drop_leading_blank(body)))?;
    let ident = |c: &mut Command, author: bool| {
        if author {
            c.env("GIT_AUTHOR_NAME", &public_name).env("GIT_AUTHOR_EMAIL", &public_email);
        }
        c.env("GIT_COMMITTER_NAME", &public_name).env("GIT_COMMITTER_EMAIL", &public_email);
    };
    let msg = message.to_string_lossy().into_owned();
    let mut c = pubc(&["commit", "-q", "--cleanup=verbatim", "-F", &msg]);
    ident(&mut c, true);
    output(&mut c)?;
    let mut c = pubc(&["tag", "-a", "--cleanup=verbatim", &tag, "-F", &msg]);
    ident(&mut c, false);
    output(&mut c)?;
    let log = output(&mut pubc(&["log", crate::release_ci::ADDRESS_LOG]))?;
    // GitHub's own addresses come with releases merged through a pull
    // request (see release_ci.rs); any other is refused, co-authors
    // included, and never printed.
    crate::public_contrib::other_addresses_on(&log, &public_email, crate::public_repo::api().as_ref(), &public_repo)
        .refuse("the public history carries", " (author, committer or Co-authored-by)")?;
    let (addrs, withheld) = crate::release_ci::printed_addresses(&log, &public_email);
    let mut addresses = addrs.join(" ");
    if withheld > 0 {
        addresses.push_str(&format!("; {withheld} withheld (accepted merges)"));
    }
    let tagger = output(&mut pubc(&["for-each-ref", "--format=%(taggeremail)", &format!("refs/tags/{tag}")]))?;
    if tagger.trim() != format!("<{public_email}>") {
        return Err("the tag carries another address".into());
    }
    let short = output(&mut pubc(&["rev-parse", "--short", "HEAD"]))?;
    let nfiles = output(&mut pubc(&["ls-files"]))?.lines().count();
    say(&format!("public commit {} tagged {tag}; {nfiles} files; addresses: {addresses}", short.trim()));

    // The wiki is committed before anything is pushed, so a wiki GitHub
    // has not created yet stops the release before the tag goes out.
    say("wiki");
    let pages = stage.join("wiki-pages");
    let names = crate::wiki::stage_pages(&tree.join("wiki"), &read_text(&tree.join("CHANGELOG.md"))?, &pages)?;
    let (n, nfiles, leaks) = leak_check(&pages, &denylist)?;
    if !leaks.is_empty() {
        for l in &leaks {
            eprintln!("release: leak: wiki/{l}");
        }
        return Err(format!("{} leak(s) in the wiki pages ({})", leaks.len(), pages.display()));
    }
    say(&format!("wiki leak check: {n} terms, {nfiles} pages, no hits"));
    let wiki_remote = crate::wiki::wiki_remote(&remote);
    let ident = crate::wiki::Ident { name: &public_name, email: &public_email };
    let wiki = crate::wiki::prepare(&pages, &wiki_remote, &public_repo, &tag, &ident, &stage.join("wiki"))?;
    if wiki.changed {
        say(&format!("wiki commit: atlas {tag} wiki, {} pages; against the wiki as it is:", names.len()));
        for l in wiki.stat.lines() {
            println!("  {l}");
        }
    } else {
        say("wiki: no page changed, nothing to commit");
    }

    say(&format!("not pushed: this run never pushes. The commit and {tag} are in {}; the wiki commit would go to {wiki_remote}.", public.display()));
    say(&format!("release notes: {}", notes_path.display()));
    say(&format!("binaries and the unsigned manifest: {}", release_dir.display()));
    if model_dir.is_some() {
        say(&format!(
            "model assets (once per model set): gh release upload <models tag> {}/* --repo {public_repo}",
            stage.join("assets").display()
        ));
    }
    Ok(())
}

/// The target a release binary's file name stands for: `atlas.exe` is
/// Windows x86_64, `atlas-<target>` the rest.
pub(crate) fn target_of(file: &str) -> Option<&str> {
    match file {
        "atlas.exe" => Some("x86_64-pc-windows-msvc"),
        f => f.strip_prefix("atlas-").filter(|t| !t.is_empty()),
    }
}

/// The release manifest (the format `atlas::update::parse_manifest` reads
/// and the updater verifies): the version and, for each binary in `bins`
/// (the launcher `atlas` skipped), its target, file name, download URL in
/// the `v<version>` release of `repo`, SHA-256 and size, in `ls` order;
/// then the notice fields of `notes` ([`ManifestNotes`]).
/// The one writer: the local run and `cargo xtask release assets`
/// both call it, and the signature covers these exact bytes.
pub(crate) fn manifest_json(version: &str, repo: &str, bins: &Path, notes: &ManifestNotes) -> R<String> {
    let mut entries = Vec::new();
    for f in ls(bins)? {
        if f == "atlas" {
            continue;
        }
        let t = target_of(&f).ok_or(format!("bin/{f}: no target for it"))?;
        let p = bins.join(&f);
        entries.push(format!(
            "{{\"target\":\"{t}\",\"file\":\"{f}\",\"url\":\"https://github.com/{repo}/releases/download/v{version}/{f}\",\"sha256\":\"{}\",\"size\":{}}}",
            sha256_file(&p)?,
            size_of(&p)?
        ));
    }
    // The notice fields after the files (issue #190); an older atlas reads
    // past them.
    let highlights: Vec<String> = notes.highlights.iter().map(|h| atlas::json::Value::String(h.clone()).to_string()).collect();
    let plugin = match &notes.plugin_changed_in {
        Some(v) => format!(",\"plugin_changed_in\":{}", atlas::json::Value::String(v.clone())),
        None => String::new(),
    };
    Ok(format!("{{\"version\":\"{version}\",\"files\":[{}],\"highlights\":[{}]{plugin}}}", entries.join(","), highlights.join(",")))
}

/// The manifest's notice fields (issue #190), signed with the rest:
/// `highlights` and `plugin_changed_in`. A manifest without them (every one
/// before) means no highlights and no plugin change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ManifestNotes {
    /// The CHANGELOG section's Highlights, bounded ([`manifest_highlights`]).
    pub(crate) highlights: Vec<String>,
    /// The release that last changed the plugin's files
    /// ([`plugin_changed_in`]); an installed plugin older than it needs
    /// `/plugin`.
    pub(crate) plugin_changed_in: Option<String>,
}

/// The paths whose files only Claude Code's `/plugin` update installs.
pub(crate) const PLUGIN_PATHS: &[&str] = &[".claude-plugin", ".mcp.json", "hooks", "skills"];

/// The manifest's `highlights`: the items of the section's Highlights (see
/// `notes_parts`), one line each, at most `atlas::update::HIGHLIGHTS_MAX`
/// of at most `atlas::update::HIGHLIGHT_CHARS` characters.
pub(crate) fn manifest_highlights(section: &str) -> Vec<String> {
    let (hi, _, _) = notes_parts(section);
    let mut items: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    let mut fence = false;
    for l in hi.lines() {
        let t = l.trim();
        if t.starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence || t.starts_with("<!--") || t.starts_with('#') {
            continue;
        }
        if t.is_empty() {
            items.extend(cur.take());
            continue;
        }
        match t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            Some(b) => {
                items.extend(cur.take());
                cur = Some(b.to_string());
            }
            None => match &mut cur {
                Some(c) => {
                    c.push(' ');
                    c.push_str(t);
                }
                None => cur = Some(t.to_string()),
            },
        }
    }
    items.extend(cur);
    items.iter().filter_map(|i| atlas::update::highlight_line(i)).take(atlas::update::HIGHLIGHTS_MAX).collect()
}

/// A plugin file as compared: `.claude-plugin/plugin.json` without its
/// version line.
fn plugin_file_view<'a>(path: &str, b: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
    if path != ".claude-plugin/plugin.json" {
        return std::borrow::Cow::Borrowed(b);
    }
    let t = String::from_utf8_lossy(b);
    let kept: Vec<&str> = t.lines().filter(|l| !l.trim_start().starts_with("\"version\":")).collect();
    std::borrow::Cow::Owned(kept.join("\n").into_bytes())
}

/// Whether two snapshots of the plugin's files (path -> bytes) differ, the
/// version line of `.claude-plugin/plugin.json` aside (every release bumps
/// it).
pub(crate) fn plugin_files_differ(old: &std::collections::BTreeMap<String, Vec<u8>>, new: &std::collections::BTreeMap<String, Vec<u8>>) -> bool {
    old.len() != new.len() || old.iter().zip(new).any(|((pa, a), (pb, b))| pa != pb || plugin_file_view(pa, a) != plugin_file_view(pb, b))
}

/// The plugin's files at `rev` in the repository at `root`.
fn plugin_snapshot(root: &Path, rev: &str) -> R<std::collections::BTreeMap<String, Vec<u8>>> {
    let mut ls = Command::new("git");
    ls.arg("-C").arg(root).args(["ls-tree", "-r", "--name-only", rev, "--"]).args(PLUGIN_PATHS);
    let names = output(&mut ls)?;
    let mut out = std::collections::BTreeMap::new();
    for n in names.lines().filter(|n| !n.is_empty()) {
        let o = Command::new("git").arg("-C").arg(root).args(["show", &format!("{rev}:{n}")]).output().map_err(|e| format!("git show: {e}"))?;
        if !o.status.success() {
            return Err(format!("git show {rev}:{n} failed"));
        }
        out.insert(n.to_string(), o.stdout);
    }
    Ok(out)
}

/// The plugin's files at one revision, as path -> bytes.
pub(crate) type PluginSnapshot = std::collections::BTreeMap<String, Vec<u8>>;

/// The manifest's `plugin_changed_in` for release `version`: walking back
/// from HEAD through the earlier `v*` tags (newest first), the release whose
/// plugin files differ from the release before it. With no earlier tag (a
/// clone without tags, or the first release), or a revision whose files
/// cannot be read (a shallow clone), it says `version` (or the newest
/// release read so far) and warns: telling users to run `/plugin` for
/// nothing is better than not telling them. A git failure never stops the
/// release.
pub(crate) fn plugin_changed_in(root: &Path, version: &str) -> R<String> {
    let want = atlas::update::parse_version(version).ok_or(format!("version {version} is not MAJOR.MINOR.PATCH"))?;
    let mut tags = Command::new("git");
    tags.arg("-C").arg(root).args(["tag", "--list", "v*"]);
    let listed = match output(&mut tags) {
        Ok(t) => t,
        Err(e) => {
            say(&format!("warning: git tag failed ({e}): the manifest says the plugin files changed in {version}"));
            return Ok(version.to_string());
        }
    };
    let mut earlier: Vec<((u64, u64, u64), String)> = listed.lines().filter_map(|t| atlas::update::parse_version(t.trim()).map(|v| (v, t.trim().to_string()))).filter(|(v, _)| *v < want).collect();
    earlier.sort();
    earlier.dedup();
    let tags: Vec<String> = earlier.into_iter().rev().map(|(_, t)| t).collect();
    Ok(plugin_history(version, &tags, |rev| plugin_snapshot(root, rev)))
}

/// [`plugin_changed_in`] over `snapshot` (HEAD is `"HEAD"`; `tags` newest
/// first).
pub(crate) fn plugin_history(version: &str, tags: &[String], snapshot: impl Fn(&str) -> R<PluginSnapshot>) -> String {
    let mut newer = version.to_string();
    let mut files = match snapshot("HEAD") {
        Ok(s) => s,
        Err(e) => {
            say(&format!("warning: the plugin files at HEAD: {e}: the manifest says they changed in {version}"));
            return newer;
        }
    };
    if tags.is_empty() {
        say(&format!("warning: no release tag before v{version} (fetch the tags): the manifest says the plugin files changed in {version}"));
        return newer;
    }
    for tag in tags {
        match snapshot(tag) {
            Ok(older) => {
                if plugin_files_differ(&older, &files) {
                    say(&format!("plugin files: last changed in {newer} (they differ from {tag})"));
                    return newer;
                }
                files = older;
                newer = tag.trim_start_matches('v').to_string();
            }
            Err(e) => {
                say(&format!("warning: the plugin files at {tag} cannot be read ({e}; a shallow clone?): the manifest says they changed in {newer}"));
                return newer;
            }
        }
    }
    say(&format!("plugin files: unchanged since {newer}, the oldest release tag"));
    newer
}

/// The manifest's notice fields for `version` from its CHANGELOG section.
pub(crate) fn manifest_notes(root: &Path, version: &str, section: &str) -> R<ManifestNotes> {
    Ok(ManifestNotes { highlights: manifest_highlights(section), plugin_changed_in: Some(plugin_changed_in(root, version)?) })
}

/// The notes' binaries table rows: platform, file and SHA-256 of each
/// binary in `bins` (the launcher and the manifest skipped).
pub(crate) fn binaries_table(bins: &Path) -> R<String> {
    let mut out = String::new();
    for f in ls(bins)? {
        if f != "atlas" && f != "manifest.json" && f != "manifest.json.sig" {
            out.push_str(&format!("| {} | bin/{f} | `{}` |\n", platform_name(&f), sha256_file(&bins.join(&f))?));
        }
    }
    Ok(out)
}

/// One model pin of config/atlas.template.json.
pub(crate) struct ModelRow {
    pub(crate) id: String,
    pub(crate) file: String,
    pub(crate) sha256: String,
    pub(crate) size: String,
}

impl ModelRow {
    /// Its line in the notes' model table.
    pub(crate) fn row(&self) -> String {
        format!("| {} | {} | {} | `{}` |\n", self.id, self.file, self.size, self.sha256)
    }
}

/// The model pins in file order (an `id` starts each; one without a file
/// is skipped).
pub(crate) fn model_rows(template: &str) -> Vec<ModelRow> {
    let mut out: Vec<ModelRow> = Vec::new();
    for (k, v) in model_lines(template) {
        if k == "id" {
            out.push(ModelRow { id: v, file: String::new(), sha256: String::new(), size: String::new() });
            continue;
        }
        let Some(m) = out.last_mut() else { continue };
        match k.as_str() {
            "file" => m.file = v,
            "sha256" => m.sha256 = v,
            _ => m.size = v,
        }
    }
    out.retain(|m| !m.file.is_empty());
    out
}

/// Files the public repository must carry; the export is refused without
/// them (an export-ignore rule or a lost file would drop them silently).
/// The rules templates are among them: atlas embeds them with
/// `include_str!`, so a public tree without them would not build.
pub const PUBLIC_FILES: &[&str] = &[
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    ".github/CODEOWNERS",
    "SECURITY.md",
    "LICENSE",
    "NOTICE",
    "README.md",
    "CHANGELOG.md",
    "CONTRIBUTING.md",
    "wiki/Home.md",
    "templates/CLAUDE.md",
    "templates/atlas-rules.md",
];

pub(crate) fn check_public_files(tree: &Path) -> R {
    let gone: Vec<&str> = PUBLIC_FILES.iter().copied().filter(|f| !tree.join(f).is_file()).collect();
    if gone.is_empty() {
        Ok(())
    } else {
        Err(format!("the export lacks {}", gone.join(", ")))
    }
}

/// The home folder as cargo finds it: USERPROFILE on Windows, HOME elsewhere.
pub(crate) fn home() -> PathBuf {
    let k = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env(k).map(PathBuf::from).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_carries_bounded_highlights_and_the_plugin_flag() {
        let section = "### Highlights\n\n<!-- a comment -->\n- Faster recall:\n  a wrapped line\n- A status line badge\n* Star item\n\n### Fixed\n\n- not a highlight";
        assert_eq!(manifest_highlights(section), vec!["Faster recall: a wrapped line", "A status line badge", "Star item"]);
        let long = format!("### Highlights\n\n{}", (0..9).map(|i| format!("- item {i} {}", "y".repeat(400))).collect::<Vec<_>>().join("\n"));
        let h = manifest_highlights(&long);
        assert_eq!(h.len(), atlas::update::HIGHLIGHTS_MAX);
        assert!(h.iter().all(|l| l.chars().count() <= atlas::update::HIGHLIGHT_CHARS && l.ends_with("...")), "{h:?}");
        // A section with no subsections is all highlights; a paragraph is one line.
        assert_eq!(manifest_highlights("One plain\nparagraph."), vec!["One plain paragraph."]);
        // Written into the manifest, they parse back as the updater reads them.
        let d = std::env::temp_dir().join(format!("atlas-xtask-notes-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("atlas.exe"), b"MZ").unwrap();
        let notes = ManifestNotes { highlights: vec!["Faster \"recall\"".into(), "Badge".into()], plugin_changed_in: Some("1.2.0".into()) };
        let m = atlas::update::parse_manifest(manifest_json("1.2.3", "o/r", &d, &notes).unwrap().as_bytes()).unwrap();
        assert_eq!(m.highlights, notes.highlights);
        assert_eq!(m.plugin_changed_in.as_deref(), Some("1.2.0"));
        let m = atlas::update::parse_manifest(manifest_json("1.2.3", "o/r", &d, &ManifestNotes::default()).unwrap().as_bytes()).unwrap();
        assert!(m.highlights.is_empty() && m.plugin_changed_in.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn plugin_changes_ignore_the_version_bump() {
        use std::collections::BTreeMap;
        let snap = |v: &str, hooks: &str| -> BTreeMap<String, Vec<u8>> {
            BTreeMap::from([
                (".claude-plugin/plugin.json".to_string(), format!("{{\n  \"name\": \"atlas\",\n  \"version\": \"{v}\",\n}}\n").into_bytes()),
                ("hooks/hooks.json".to_string(), hooks.as_bytes().to_vec()),
            ])
        };
        assert!(!plugin_files_differ(&snap("0.5.0", "{}"), &snap("0.5.1", "{}")), "a version bump alone is no plugin change");
        assert!(plugin_files_differ(&snap("0.5.0", "{}"), &snap("0.5.1", "{\"hooks\":{}}")), "a hooks edit is");
        let mut added = snap("0.5.1", "{}");
        added.insert("skills/new/SKILL.md".into(), b"x".to_vec());
        assert!(plugin_files_differ(&snap("0.5.0", "{}"), &added), "a new skill is");
        let mut gone = snap("0.5.0", "{}");
        gone.remove("hooks/hooks.json");
        assert!(plugin_files_differ(&snap("0.5.0", "{}"), &gone), "a removed file is");
    }

    #[test]
    fn plugin_changed_in_is_the_release_that_last_changed_the_plugin() {
        use std::collections::BTreeMap;
        fn snap(v: &str, hooks: &str) -> PluginSnapshot {
            BTreeMap::from([
                (".claude-plugin/plugin.json".to_string(), format!("{{\n  \"version\": \"{v}\",\n}}\n").into_bytes()),
                ("hooks/hooks.json".to_string(), hooks.as_bytes().to_vec()),
            ])
        }
        // Tags newest first; 0.5.1 changed the hooks 0.5.0 and 0.4.0 had.
        let tags: Vec<String> = ["v0.5.1", "v0.5.0", "v0.4.0"].iter().map(|t| t.to_string()).collect();
        fn history(head: &str, unreadable: Option<&str>) -> impl Fn(&str) -> R<PluginSnapshot> {
            let (head, unreadable) = (head.to_string(), unreadable.map(str::to_string));
            move |rev: &str| -> R<PluginSnapshot> {
                if unreadable.as_deref() == Some(rev) {
                    return Err(format!("git show {rev} failed"));
                }
                Ok(match rev {
                    "HEAD" => snap("0.6.0", &head),
                    "v0.5.1" => snap("0.5.1", "{b}"),
                    _ => snap(rev, "{a}"),
                })
            }
        }
        assert_eq!(plugin_history("0.6.0", &tags, history("{b}", None)), "0.5.1", "unchanged since 0.5.1: a plugin at 0.5.1 needs nothing");
        assert_eq!(plugin_history("0.6.0", &tags, history("{c}", None)), "0.6.0", "this release changes them");
        let same = |rev: &str| -> R<PluginSnapshot> { Ok(snap(rev, "{a}")) };
        assert_eq!(plugin_history("0.6.0", &tags, same), "0.4.0", "unchanged since the oldest tag");
        // Fail soft: a revision that cannot be read (a shallow clone) names
        // the newest release read so far; no tags names this release.
        assert_eq!(plugin_history("0.6.0", &tags, history("{b}", Some("v0.5.0"))), "0.5.1");
        assert_eq!(plugin_history("0.6.0", &tags, history("{b}", Some("v0.5.1"))), "0.6.0");
        assert_eq!(plugin_history("0.6.0", &tags, history("{b}", Some("HEAD"))), "0.6.0");
        assert_eq!(plugin_history("0.6.0", &[], same), "0.6.0");
    }

    #[test]
    fn signing_is_checked_before_the_build() {
        let dir = std::env::temp_dir().join(format!("atlas-xtask-sign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("test-key.pem");
        std::fs::write(&key, "not a key").unwrap();
        let tool = dir.join("openssl");
        let k = key.to_str().unwrap();
        assert!(signing_ready(Some(k), Some(&tool)).is_ok());
        assert!(signing_ready(Some(k), None).unwrap_err().contains("openssl"));
        assert!(signing_ready(None, Some(&tool)).unwrap_err().contains("ATLAS_SIGN_KEY"));
        let missing = dir.join("absent.pem");
        assert!(signing_ready(missing.to_str(), Some(&tool)).unwrap_err().contains("not a file"));
        assert!(signing_ready(dir.to_str(), Some(&tool)).unwrap_err().contains("not a file"), "a folder is no key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changelog_section_is_the_tagged_one() {
        let c = "# Changelog\n\nintro\n\n## v0.2.0\n\n- two\n\n## v0.1.0\n\n- one\n- more\n\n";
        assert_eq!(changelog_section(c, "v0.1.0"), "- one\n- more");
        assert_eq!(changelog_section(c, "v0.2.0"), "- two");
        assert_eq!(changelog_section(c, "v0.3.0"), "");
    }

    /// A fenced block is opaque to the readers, as to the fragment parsers:
    /// a `## v` line inside one, indented up to three spaces, backticks or
    /// tildes, neither ends the section nor starts one.
    #[test]
    fn changelog_section_reads_past_fenced_version_lines() {
        for (open, close) in [("```", "```"), ("  ```", "  ```"), ("   ~~~", "~~~~"), ("````sh", "````")] {
            let c = format!("# Changelog\n\n## v0.2.0\n\n- two\n{open}\n## v0.0.1\n- fake\n{close}\n- after\n\n## v0.1.0\n\n- one\n");
            assert_eq!(changelog_section(&c, "v0.2.0"), format!("- two\n{open}\n## v0.0.1\n- fake\n{close}\n- after"), "{open}");
            assert_eq!(changelog_section(&c, "v0.0.1"), "", "{open}: a fenced line is no section");
            assert_eq!(changelog_section(&c, "v0.1.0"), "- one");
        }
        // A shorter run, or the other character, does not close a fence.
        let c = "## v0.2.0\n\n- two\n````\n```\n## v0.0.1\n~~~~\n````\n\n## v0.1.0\n\n- one\n";
        assert_eq!(changelog_section(c, "v0.2.0"), "- two\n````\n```\n## v0.0.1\n~~~~\n````");
        // Four spaces or a tab is not a fence (CommonMark): the line ends it.
        let c = "## v0.2.0\n\n- two\n    ```\n## v0.0.1\n";
        assert_eq!(changelog_section(c, "v0.2.0"), "- two\n    ```");
        // An inline code span at the start of a line opens no fence.
        let c = "## v0.2.0\n\n```a``` b\n\n## v0.1.0\n\n- one\n";
        assert_eq!(changelog_section(c, "v0.2.0"), "```a``` b");
        // A fence left open swallows the rest, and the release refuses it.
        let c = "## v0.2.0\n\n### Highlights\n\n- two\n  ```\n\n## v0.1.0\n\n- one\n";
        let s = changelog_section(c, "v0.2.0");
        assert!(s.ends_with("- one"), "{s}");
        assert!(check_section(&s, "v0.2.0").unwrap_err().contains("unclosed fenced block"));
    }

    /// A line indented less than the fence that opened (a list item's fence
    /// whose item ended) breaks the block on GitHub: the release refuses
    /// the section, its closing line included; blank lines and lines
    /// indented at least as far are fine.
    #[test]
    fn check_section_refuses_a_line_indented_less_than_its_fence() {
        let ok = "### Highlights\n\n- a\n  ```\n  x\n\n    y\n\t z\n  ```\n- b";
        check_section(ok, "v1").unwrap();
        for bad in [
            "### Highlights\n\n- a\n  ```\n- b\n## v0.0.1\n```\n",
            "### Highlights\n\n- a\n   ~~~\n  x\n   ~~~",
            "### Highlights\n\n- a\n  ```\n  x\n```",
            "### Highlights\n\n- a\n  ```\n x\n  ```",
        ] {
            let e = check_section(bad, "v1").unwrap_err();
            assert!(e.contains("indented less than"), "{bad:?}: {e}");
        }
        let mut f = Fence::default();
        for l in ["- a", "  ```", "  x", ""] {
            f.opaque(l);
        }
        assert!(f.open() && !f.short());
        f.opaque("x");
        assert!(f.short(), "a short line marks the fence");
    }

    /// Outside a fence the released section holds no line GitHub reads as
    /// HTML (comments included), no control character but a tab or a line
    /// ending, and no heading but its own `### ` subsections: none after an
    /// indent or a list marker, and no setext underline.
    #[test]
    fn check_section_refuses_html_controls_and_nested_headings() {
        let ok = "### Highlights\n\n- a\tb, fixes #12\n  #5 and C# too\n- #7\n  a -- b = c, x < y\n  ```\n  <!-- x -->\n  # y\n  ===\n  - ## v9.9.9\n  ```\n- b";
        check_section(ok, "v1").unwrap();
        for (bad, want) in [
            ("### Highlights\n\n- a\n<!-- x -->", "starts with <"),
            ("### Highlights\n\n<!-- todo -->\n- a", "starts with <"),
            ("### Highlights\n\n- a\n  <details>\n\n- b\n  </details>", "starts with <"),
            ("### Highlights\n\n- a\n\t<br>", "starts with <"),
            ("### Highlights\n\n- a\r<!--\n- b", "control character"),
            ("### Highlights\n\n- a\r## v0.0.1", "control character"),
            ("### Highlights\n\n- a\r```", "control character"),
            ("### Highlights\n\n- a \u{1b}[2J", "control character"),
            ("### Highlights\n\n- a\u{0}", "control character"),
            ("### Highlights\n\n- ## v9.9.9", "heading"),
            ("### Highlights\n\n- a\n  ## v9.9.9", "heading"),
            ("### Highlights\n\n1. # x", "heading"),
            ("### Highlights\n\n- a\n  > ### x", "heading"),
            ("### Highlights\n\n- v9.9.9\n  ===", "underline"),
            ("### Highlights\n\nText\n---\n\n- a", "underline"),
        ] {
            let e = check_section(bad, "v1").unwrap_err();
            assert!(e.contains(want) && e.starts_with("the v1 section of CHANGELOG.md"), "{bad:?}: {e}");
        }
    }

    /// `###` inside an indented or tilde fence is not a subsection.
    #[test]
    fn subsections_skip_every_fence_the_parsers_skip() {
        for (open, close) in [("  ```", "  ```"), ("~~~", "~~~"), ("   ```sh", "```")] {
            let s = format!("### Highlights\n\n- a\n{open}\n### Features\n{close}\n- b");
            assert_eq!(subsections(&s), [("Highlights".to_string(), format!("- a\n{open}\n### Features\n{close}\n- b"))], "{open}");
        }
    }

    #[test]
    fn versions_and_keys_read_as_sed_read_them() {
        assert_eq!(cargo_version("[package]\nname = \"atlas\"\nversion = \"0.1.0\"\n").as_deref(), Some("0.1.0"));
        assert_eq!(plugin_version("{\n  \"version\": \"0.1.0\",\n}").as_deref(), Some("0.1.0"));
        let k = "a".repeat(64);
        assert_eq!(key_const(&format!("pub const TEST_KEY_HEX: &str = \"{k}\"; // x\n"), "TEST_KEY_HEX"), Some(k));
        assert_eq!(key_const("pub const TEST_KEY_HEX: &str = \"abc\";\n", "TEST_KEY_HEX"), None);
    }

    #[test]
    fn model_lines_take_plain_values() {
        let t = "{\n  \"models\": [\n    {\n      \"id\": \"embed\",\n      \"file\": \"m.atm\",\n      \"size\": 12\n    }\n  ]\n}";
        let got = model_lines(t);
        assert_eq!(got, [("id".to_string(), "embed".to_string()), ("file".into(), "m.atm".into()), ("size".into(), "12".into())]);
    }

    #[test]
    fn collation_puts_exe_between_the_aarch64_and_x86_64_names() {
        let mut v = vec!["atlas-x86_64-unknown-linux-gnu", "atlas.exe", "atlas", "atlas-aarch64-apple-darwin"];
        v.sort_by_key(|s| collate_key(s));
        assert_eq!(v, ["atlas", "atlas-aarch64-apple-darwin", "atlas.exe", "atlas-x86_64-unknown-linux-gnu"]);
    }

    #[test]
    fn terms_match_normalised_in_bytes() {
        let db = crate::ucd::Db::load(&crate::pack_embedder::default_ucd()).unwrap();
        let m = crate::leak::Matcher::new(&db, "# c\nsecret\n");
        let hit = |b: &[u8]| m.hits(&[("a.bin".into(), b.to_vec())]);
        assert_eq!(hit(b"\x00\xffSecret\x00"), vec!["a.bin:1:3 (byte 2): denylist term #1".to_string()]);
        assert!(hit(b"secre t").is_empty());
        let wide: Vec<u8> = "xx SECRET".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(hit(&wide).len(), 1, "a UTF-16 string in a binary");
    }

    #[test]
    fn notes_fill_whole_line_placeholders() {
        let t = "# atlas @VERSION@\n\n@CHANGES@\n\n| a |\n@BINARIES@\n\n@MISSING@\n\n@MODELS@\nend @REPO@ @DATE@\n";
        let n = Notes { tag: "v1", date: "2026-11-02", repo: "o/r", changes: "- c", binaries: "| b |\n", missing: "", models: "" };
        // An empty block takes its line and the blank line after it.
        assert_eq!(fill_notes(t, &n), "# atlas v1\n\n- c\n\n| a |\n| b |\n\nend o/r 2026-11-02\n");
    }

    const SECTION: &str = "### Highlights\n\n- fast\n\n### Features\n\n- a\n\n### Upgrade notes\n\n- run setup\n\n### Known limits\n\n- b";

    #[test]
    fn notes_lead_with_highlights_then_upgrade_then_details() {
        let (hi, up, rest) = notes_parts(SECTION);
        assert_eq!(hi, "- fast");
        assert_eq!(up, "## Upgrade notes\n\n- run setup");
        assert_eq!(rest, "### Features\n\n- a\n\n### Known limits\n\n- b");
        let t = "# atlas @VERSION@\n\n@HIGHLIGHTS@\n\nobsidian\n\n@UPGRADE@\n\n## Install\n\n@DETAILS@\n\nend\n";
        let n = Notes { tag: "v1", date: "d", repo: "o/r", changes: SECTION, binaries: "", missing: "", models: "" };
        assert_eq!(
            fill_notes(t, &n),
            "# atlas v1\n\n- fast\n\nobsidian\n\n## Upgrade notes\n\n- run setup\n\n## Install\n\n<details>\n<summary>Everything in v1</summary>\n\n### Features\n\n- a\n\n### Known limits\n\n- b\n\n</details>\n\nend\n"
        );
        // No upgrade part and no details: their lines and the blank after each go.
        let n = Notes { changes: "### Highlights\n\n- fast", ..n };
        assert_eq!(fill_notes(t, &n), "# atlas v1\n\n- fast\n\nobsidian\n\n## Install\n\nend\n");
        // A section without subsections is all highlights.
        assert_eq!(notes_parts("- one\n- two").0, "- one\n- two");
    }

    #[test]
    fn the_release_refuses_an_empty_or_highlightless_section() {
        assert!(check_section(SECTION, "v1").is_ok());
        assert!(check_section("- one", "v1").is_ok());
        assert!(check_section("", "v1").unwrap_err().contains("no v1 section"));
        assert!(check_section("\n   \n<!-- write me -->\n### Highlights\n\n", "v1").unwrap_err().contains("is empty"));
        assert!(check_section("### Features\n\n- a", "v1").unwrap_err().contains("no ### Highlights"));
        assert!(check_section("### Highlights\n\n\n### Features\n\n- a", "v1").unwrap_err().contains("no ### Highlights"));
        // A ### line inside a fence is not a subsection.
        assert_eq!(subsections("### Highlights\n\n```\n### not a heading\n```").len(), 1);
    }

    /// The shipped CHANGELOG.md and template: the current version's section
    /// passes the rule and the notes come out in the documented order.
    /// ATLAS_NOTES_SAMPLE names a file to write the rendered notes to.
    #[test]
    fn the_shipped_changelog_and_template_make_notes() {
        let root = repo_root();
        let version = cargo_version(&read_text(&root.join("Cargo.toml")).unwrap()).unwrap();
        let tag = format!("v{version}");
        let log = read_text(&root.join("CHANGELOG.md")).unwrap();
        let section = changelog_section(&log, &tag);
        check_section(&section, &tag).unwrap();
        // Every released section passes the rule too: none holds a comment
        // or other placeholder the release would now refuse.
        let tags: Vec<&str> = log.lines().filter_map(|l| l.strip_prefix("## ")).filter(|t| t.starts_with('v')).collect();
        assert!(tags.len() > 1 && tags.contains(&tag.as_str()), "{tags:?}");
        for t in tags {
            check_section(&changelog_section(&log, t), t).unwrap();
        }
        let t = read_text(&root.join("scripts/release-notes.md.in")).unwrap();
        let n = Notes {
            tag: &tag,
            date: "2026-11-02",
            repo: DEFAULT_PUBLIC_REPO,
            changes: &section,
            binaries: "| Windows x86_64 | bin/atlas.exe | `0000` |\n",
            missing: "",
            models: "| embed | embed.atm | 1 | `0000` |\n",
        };
        let out = fill_notes(&t, &n);
        for p in ["@VERSION@", "@DATE@", "@REPO@", "@CHANGES@", "@HIGHLIGHTS@", "@UPGRADE@", "@DETAILS@", "@BINARIES@", "@MISSING@", "@MODELS@"] {
            assert!(!out.contains(p), "{p} left in the notes");
        }
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s:?} not in the notes:\n{out}"));
        assert!(at("Released 2026-11-02") < at("## Highlights"));
        assert!(at("## Highlights") < at("You do not need Obsidian"));
        assert!(at("You do not need Obsidian") < at("## Install"));
        assert!(at("## Install") < at("## Prebuilt binaries"));
        assert!(at("## Model files") < at("https://github.com/bloodfeast/atlas/wiki/Changelog"));
        if let Some(f) = env("ATLAS_NOTES_SAMPLE") {
            write(Path::new(&f), &out).unwrap();
        }
    }

    #[test]
    fn the_export_must_carry_the_public_files() {
        let d = std::env::temp_dir().join(format!("atlas-public-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for f in PUBLIC_FILES {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        assert!(check_public_files(&d).is_ok());
        std::fs::remove_file(d.join(".github/CODEOWNERS")).unwrap();
        assert_eq!(check_public_files(&d).unwrap_err(), "the export lacks .github/CODEOWNERS");
        let _ = std::fs::remove_dir_all(&d);
        // This repository tracks them, and no export-ignore rule drops them,
        // so `git archive HEAD` (the export) carries them.
        let root = repo_root();
        let tracked = output(Command::new("git").arg("-C").arg(&root).args(["ls-files"])).unwrap();
        let attrs = output(Command::new("git").arg("-C").arg(&root).args(["check-attr", "export-ignore", "--"]).args(PUBLIC_FILES)).unwrap();
        for f in PUBLIC_FILES {
            assert!(tracked.lines().any(|l| l == *f), "{f} is not tracked");
            assert!(attrs.lines().any(|l| l == format!("{f}: export-ignore: unspecified")), "{f} is export-ignored: {attrs}");
        }
    }
}
