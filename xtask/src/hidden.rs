//! `cargo xtask hidden`: no tracked file may hold a character a reviewer
//! cannot see. Run by the public CI's `test` job (after Test, ubuntu), by
//! an xtask unit test (so every local `cargo test` runs it), by the
//! release-pr job beside the leak and secret gates, by `cargo xtask release
//! check`, and by `cargo xtask release` on the exported tree. No network.
//!
//! A finding is one of:
//!
//! - an invisible character, from the one table the runtime scrub uses
//!   (`atlas::hidden::marks`): bidi controls, zero-width characters, tag
//!   characters, Hangul fillers, variation selectors (VS1 to VS16 and the
//!   supplement) and the rest of `Default_Ignorable_Code_Point`. In prose
//!   (`.md`, `.txt` and the like) the few real text needs in their place (a
//!   lone joiner between two non-ASCII characters, a lone bidi mark, one
//!   basic variation selector straight after its base, an RGI subdivision
//!   flag) pass, as they pass the scrub; in code ([`CODE`]) none does, so
//!   `a` + U+FE00, a name that looks like `a`, is a finding. U+FEFF at
//!   byte 0 is a byte order mark and passes;
//! - a control character other than tab and line feed, a carriage return
//!   included unless a line feed follows it (a lone CR hides the start of
//!   a line in a terminal);
//! - a byte sequence that is not UTF-8, in a file that is not binary (one
//!   stray byte must not hide the rest of the file from the gate);
//! - in code, one token (a run of letters, digits and `_`) that mixes Latin
//!   letters with Cyrillic or Greek ones, the look-alike spelling of a name.
//!
//! Each is reported `path:line:column: U+XXXX what` (column in characters).
//! Binary files (a NUL byte, or a binary extension) are skipped.
//!
//! # The allowlist
//!
//! [`ALLOW`] (checked in) lets the deliberate cases through, one line per
//! entry: `path:LINES:U+XXXX reason`, where LINES is a comma list of line
//! numbers and `A-B` ranges. `#` comments and blank lines are ignored. A
//! reason is required, every listed line must hold the character (a line
//! that does not is stale, an error), and an invisible character in code is
//! never allowlisted: write an escape.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use atlas::hidden::Act;

type R<T = ()> = Result<T, String>;

/// The allowlist, relative to the workspace root.
pub const ALLOW: &str = "xtask/hidden-allow.txt";

/// Extensions of code: no invisible character passes, and mixed-script
/// tokens are checked.
pub const CODE: &[&str] = &["rs", "yml", "yaml", "toml", "sh", "ps1", "json", "py", "js", "mjs", "ts", "go", "java", "cs", "csproj", "xml"];

/// Extensions of binary files the gate does not read.
const BINARY: &[&str] =
    &["png", "jpg", "jpeg", "gif", "ico", "webp", "pdf", "zip", "gz", "wasm", "sqlite", "onnx", "bin", "ttf", "woff", "woff2", "scip"];

fn ext(path: &str) -> &str {
    path.rsplit_once('.').map(|(_, e)| e).filter(|e| !e.contains('/')).unwrap_or("")
}

fn is_code(path: &str) -> bool {
    CODE.contains(&ext(path))
}

/// One hidden character, one mixed-script token or one invalid byte run in
/// a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    /// 1-based line.
    pub line: usize,
    /// 1-based column, in characters.
    pub col: usize,
    /// The character reported (for a mixed token, its first non-Latin
    /// letter; for invalid UTF-8, U+FFFD).
    pub cp: u32,
    pub what: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}: U+{:04X} {}", self.path, self.line, self.col, self.cp, self.what)
    }
}

/// What a hidden or control character is, for the report.
pub fn kind(c: char) -> &'static str {
    match c as u32 {
        0x0D => "lone carriage return",
        _ if c.is_control() => "control character",
        0x202A..=0x202E | 0x2066..=0x2069 | 0x200E | 0x200F | 0x61C => "bidi control",
        0x200B..=0x200D | 0x2060 | 0xFEFF => "zero-width character",
        0xFE00..=0xFE0F | 0xE0100..=0xE01EF | 0x180B..=0x180D | 0x180F => "variation selector",
        0xE0000..=0xE0FFF => "tag character",
        0x115F | 0x1160 | 0x3164 | 0xFFA0 => "Hangul filler",
        _ => "invisible character",
    }
}

/// The script of a letter, as far as look-alikes go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Script {
    Latin,
    Cyrillic,
    Greek,
}

fn script(c: char) -> Option<Script> {
    let u = c as u32;
    match u {
        0x41..=0x5A
        | 0x61..=0x7A
        | 0xC0..=0xD6
        | 0xD8..=0xF6
        | 0xF8..=0x24F
        | 0x1E00..=0x1EFF
        | 0x2C60..=0x2C7F
        | 0xA720..=0xA7FF
        | 0xFF21..=0xFF3A
        | 0xFF41..=0xFF5A => Some(Script::Latin),
        0x400..=0x52F | 0x1C80..=0x1C8F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F => Some(Script::Cyrillic),
        0x370..=0x3FF | 0x1F00..=0x1FFF => Some(Script::Greek),
        _ => None,
    }
}

/// `bytes` as text, and the byte offsets in that text where an invalid
/// UTF-8 run stood (each written U+FFFD).
fn decode(bytes: &[u8]) -> (String, Vec<usize>) {
    if let Ok(t) = std::str::from_utf8(bytes) {
        return (t.to_string(), Vec::new());
    }
    let mut text = String::with_capacity(bytes.len());
    let mut bad = Vec::new();
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        if !chunk.invalid().is_empty() {
            bad.push(text.len());
            text.push('\u{FFFD}');
        }
    }
    (text, bad)
}

/// Every finding in one file's bytes.
pub fn scan(path: &str, bytes: &[u8]) -> Vec<Finding> {
    let (text, bad) = decode(bytes);
    let code = is_code(path);
    // Byte offset to (line, column), both 1-based, column in characters.
    let mut at = Vec::new();
    for m in atlas::hidden::marks(&text) {
        let report = match m.act {
            Act::Drop | Act::Show => !(m.at == 0 && m.c == '\u{FEFF}'),
            Act::Kept => code,
            Act::LineEnd => false,
        };
        if report {
            at.push((m.at, m.c as u32, kind(m.c).to_string()));
        }
    }
    at.extend(bad.iter().map(|&b| (b, 0xFFFD, "invalid UTF-8".to_string())));
    let mut out: Vec<Finding> = Vec::new();
    let place = |byte: usize| -> (usize, usize) {
        let before = &text[..byte];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        (line, col)
    };
    for (b, cp, what) in at {
        let (line, col) = place(b);
        out.push(Finding { path: path.to_string(), line, col, cp, what });
    }
    if code {
        mixed_tokens(path, &text, &mut out);
    }
    out.sort_by_key(|f| (f.line, f.col));
    out
}

/// The tokens of `text` that mix Latin letters with Cyrillic or Greek.
fn mixed_tokens(path: &str, text: &str, out: &mut Vec<Finding>) {
    let (mut line, mut col) = (1usize, 0usize);
    let mut scripts = BTreeSet::new();
    let mut odd: Option<(usize, usize, char)> = None;
    let mut flush = |scripts: &mut BTreeSet<Script>, odd: &mut Option<(usize, usize, char)>| {
        if scripts.contains(&Script::Latin) && scripts.len() > 1 {
            if let Some((l, c, ch)) = *odd {
                let names: Vec<&str> = scripts
                    .iter()
                    .map(|s| match s {
                        Script::Latin => "Latin",
                        Script::Cyrillic => "Cyrillic",
                        Script::Greek => "Greek",
                    })
                    .collect();
                out.push(Finding {
                    path: path.to_string(),
                    line: l,
                    col: c,
                    cp: ch as u32,
                    what: format!("mixed-script token ({})", names.join("+")),
                });
            }
        }
        scripts.clear();
        *odd = None;
    };
    for ch in text.chars() {
        col += 1;
        if ch.is_alphanumeric() || ch == '_' {
            if let Some(s) = script(ch) {
                scripts.insert(s);
                if s != Script::Latin && odd.is_none() {
                    odd = Some((line, col, ch));
                }
            }
        } else {
            flush(&mut scripts, &mut odd);
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        }
    }
    flush(&mut scripts, &mut odd);
}

/// The parsed allowlist: one (path, line, code point) per listed line.
#[derive(Debug, Default)]
pub struct Allow {
    pub entries: Vec<(String, usize, u32)>,
}

impl Allow {
    /// Parses the allowlist text; every entry needs a reason, and an
    /// invisible character in code is refused.
    pub fn parse(text: &str) -> R<Allow> {
        let mut entries = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let l = raw.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let bad = || format!("{ALLOW}:{}: want `path:LINES:U+XXXX reason`, got {l:?}", n + 1);
            let (entry, reason) = l.split_once([' ', '\t']).ok_or_else(bad)?;
            if reason.trim().is_empty() {
                return Err(bad());
            }
            let (rest, cp) = entry.rsplit_once(':').ok_or_else(bad)?;
            let (path, lines) = rest.rsplit_once(':').ok_or_else(bad)?;
            let cp = u32::from_str_radix(cp.strip_prefix("U+").ok_or_else(bad)?, 16).map_err(|_| bad())?;
            let c = char::from_u32(cp).ok_or_else(bad)?;
            if is_code(path) && !c.is_control() {
                return Err(format!(
                    "{ALLOW}:{}: {path}: an invisible character in code is never allowlisted; write it as an escape",
                    n + 1
                ));
            }
            for part in lines.split(',') {
                let (a, b) = part.split_once('-').unwrap_or((part, part));
                let (a, b): (usize, usize) = (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?);
                if a == 0 || b < a {
                    return Err(bad());
                }
                entries.extend((a..=b).map(|line| (path.to_string(), line, cp)));
            }
        }
        Ok(Allow { entries })
    }
}

/// The findings in `files` the allowlist does not let through, and the
/// allowlist entries that matched nothing.
pub fn check(files: &[(String, Vec<u8>)], allow: &Allow) -> (Vec<Finding>, Vec<(String, usize, u32)>) {
    let mut left = Vec::new();
    let mut used = vec![false; allow.entries.len()];
    for (path, bytes) in files {
        if bytes.contains(&0) || BINARY.contains(&ext(path)) {
            continue;
        }
        for f in scan(path, bytes) {
            match allow.entries.iter().position(|(p, l, c)| *p == f.path && *l == f.line && *c == f.cp) {
                Some(i) => used[i] = true,
                None => left.push(f),
            }
        }
    }
    let stale = allow.entries.iter().zip(used).filter(|(_, u)| !u).map(|(e, _)| e.clone()).collect();
    (left, stale)
}

/// The tracked files under `root`, read.
fn tracked(root: &Path) -> R<Vec<(String, Vec<u8>)>> {
    let out = Command::new("git").arg("-C").arg(root).args(["ls-files", "-z"]).output().map_err(|e| format!("git ls-files: {e}"))?;
    if !out.status.success() {
        return Err(format!("git ls-files failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut files = Vec::new();
    for p in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let p = String::from_utf8_lossy(p).into_owned();
        // A tracked file deleted in the work tree is not there to read.
        match std::fs::read(root.join(&p)) {
            Ok(b) => files.push((p, b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{p}: {e}")),
        }
    }
    Ok(files)
}

/// The allowlist at `root`.
pub fn allowlist(root: &Path) -> R<Allow> {
    let p = root.join(ALLOW);
    Allow::parse(&std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?)
}

/// The gate over `files`: Ok with a one-line summary, or Err listing every
/// finding (and, when `strict`, every allowlist entry that matched nothing).
pub fn gate(files: &[(String, Vec<u8>)], allow: &Allow, strict: bool, what: &str) -> R<String> {
    let (left, stale) = check(files, allow);
    let stale: Vec<_> = if strict { stale } else { Vec::new() };
    if left.is_empty() && stale.is_empty() {
        return Ok(format!(
            "context poisoning guard: {} files in {what}, no hidden text ({} allowlisted lines)",
            files.len(),
            allow.entries.len()
        ));
    }
    let mut msg = String::new();
    for f in &left {
        msg.push_str(&format!("{f}\n"));
    }
    for (p, l, c) in &stale {
        msg.push_str(&format!("{ALLOW}: {p}:{l}:U+{c:04X} matches nothing; remove it\n"));
    }
    msg.push_str(&format!(
        "context poisoning guard: {} hidden-text finding(s), {} stale allowlist line(s) in {what}; a deliberate case goes in {ALLOW} with its reason",
        left.len(),
        stale.len()
    ));
    Err(msg)
}

/// `cargo xtask hidden`: every tracked file of the workspace.
pub fn run(args: Vec<String>) -> R {
    if !args.is_empty() {
        return Err("usage: cargo xtask hidden".into());
    }
    println!("{}", tracked_gate(&crate::skill::root())?);
    Ok(())
}

/// The gate over every tracked file under `root` (stale allowlist entries
/// are errors): its summary line with the time it took.
pub fn tracked_gate(root: &Path) -> R<String> {
    let started = Instant::now();
    let files = tracked(root)?;
    let allow = allowlist(root)?;
    let summary = gate(&files, &allow, true, "the tracked tree")?;
    Ok(format!("{summary}, {:.2}s", started.elapsed().as_secs_f64()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(path: &str, text: &str) -> Vec<String> {
        scan(path, text.as_bytes()).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn each_class_is_found_with_its_place() {
        assert_eq!(one("a.md", "ok\nab\u{202E}c"), ["a.md:2:3: U+202E bidi control"]);
        assert_eq!(one("a.toml", "x = 1\u{2067}"), ["a.toml:1:6: U+2067 bidi control"]);
        assert_eq!(one("a.json", "{\"k\u{200B}\": 1}"), ["a.json:1:4: U+200B zero-width character"]);
        assert_eq!(one("a.yml", "on:\u{2060}"), ["a.yml:1:4: U+2060 zero-width character"]);
        assert_eq!(one("changes/1.md", "- x\u{E0041}"), ["changes/1.md:1:4: U+E0041 tag character"]);
        assert_eq!(one("a.rs", "// \u{AD}"), ["a.rs:1:4: U+00AD invisible character"]);
        assert_eq!(one("a.txt", "x\u{1b}[2J"), ["a.txt:1:2: U+001B control character"]);
        assert_eq!(one("a.txt", "x\u{E0100}"), ["a.txt:1:2: U+E0100 variation selector"]);
        assert_eq!(one("a.js", "const \u{3164} = 1"), ["a.js:1:7: U+3164 Hangul filler"]);
        assert_eq!(one("a.md", "a\u{34F}b\u{E0080}"), ["a.md:1:2: U+034F invisible character", "a.md:1:4: U+E0080 tag character"]);
    }

    #[test]
    fn a_bom_is_allowed_at_byte_zero_and_crlf_everywhere() {
        assert!(one("a.md", "\u{FEFF}# x").is_empty());
        assert_eq!(one("a.md", "# x\u{FEFF}"), ["a.md:1:4: U+FEFF zero-width character"]);
        assert!(one("a.md", "tab\there\r\nok\n").is_empty());
    }

    #[test]
    fn a_lone_carriage_return_is_found() {
        assert_eq!(one("a.sh", "echo ok\rrm -rf /\n"), ["a.sh:1:8: U+000D lone carriage return"]);
    }

    #[test]
    fn invalid_utf8_is_found_and_the_rest_still_read() {
        let f = scan("run.sh", b"# \xFF comment\necho \xE2\x80\xAE x\n");
        let f: Vec<String> = f.iter().map(ToString::to_string).collect();
        assert_eq!(f, ["run.sh:1:3: U+FFFD invalid UTF-8", "run.sh:2:6: U+202E bidi control"]);
    }

    #[test]
    fn prose_keeps_what_real_text_needs_and_code_does_not() {
        let persian = "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}";
        let family = "\u{1F468}\u{200D}\u{1F469}";
        assert!(one("a.md", persian).is_empty() && one("a.md", family).is_empty());
        assert_eq!(one("a.rs", &format!("// {persian}")), ["a.rs:1:6: U+200C zero-width character"]);
        // A run of joiners is never real text.
        assert_eq!(
            one("a.md", "\u{645}\u{200C}\u{200C}\u{62E}"),
            ["a.md:1:2: U+200C zero-width character", "a.md:1:3: U+200C zero-width character"]
        );
    }

    #[test]
    fn a_tag_flag_or_selector_run_is_found_in_prose_and_any_selector_in_code() {
        // U+1F3F4 + tags "ignore" + U+E007F: not an RGI flag.
        let planted = "\u{1F3F4}\u{E0069}\u{E0067}\u{E006E}\u{E006F}\u{E0072}\u{E0065}\u{E007F}";
        let f = one("notes.md", planted);
        assert_eq!(f.len(), 7, "{f:?}");
        assert_eq!(f[0], "notes.md:1:2: U+E0069 tag character");
        let wales = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0077}\u{E006C}\u{E0073}\u{E007F}";
        let heart = "\u{2764}\u{FE0F}\u{200D}\u{1F525}";
        assert!(one("a.md", wales).is_empty() && one("a.md", heart).is_empty());
        // A selector run, and the ZWJ/VS16 chain, in prose.
        assert_eq!(one("a.md", "\u{8C48}\u{FE00}\u{FE01}"), ["a.md:1:3: U+FE01 variation selector"]);
        // A selector its base does not define, in prose.
        assert_eq!(one("a.md", "\u{845B}\u{FE05}"), ["a.md:1:2: U+FE05 variation selector"]);
        assert_eq!(
            one("a.md", "x\u{FE0F}\u{200D}\u{FE0F}"),
            ["a.md:1:2: U+FE0F variation selector", "a.md:1:3: U+200D zero-width character", "a.md:1:4: U+FE0F variation selector"]
        );
        // In code: the look-alike name, and even a selector after its base.
        assert_eq!(one("a.js", "const a\u{FE00} = 1"), ["a.js:1:8: U+FE00 variation selector"]);
        assert_eq!(one("a.py", "s = \"\u{2764}\u{FE0F}\""), ["a.py:1:7: U+FE0F variation selector"]);
    }

    #[test]
    fn mixed_script_tokens_in_code_only() {
        // `p` + Cyrillic `а` + `ssword`: one token, two scripts.
        assert_eq!(one("a.rs", "let p\u{430}ssword = 1;"), ["a.rs:1:6: U+0430 mixed-script token (Latin+Cyrillic)"]);
        assert_eq!(one("a.rs", "fn \u{3BF}k() {}"), ["a.rs:1:4: U+03BF mixed-script token (Latin+Greek)"]);
        assert_eq!(one("ci.yml", "url: https://ex\u{430}mple.com"), ["ci.yml:1:16: U+0430 mixed-script token (Latin+Cyrillic)"]);
        // One script per token is fine, and a .md file is not tokenised.
        assert!(one("a.rs", "let \u{3B1}\u{3B2} = x_y; // \u{43F}\u{440}\u{438}\u{432}\u{435}\u{442} hello").is_empty());
        assert!(one("a.md", "p\u{430}ssword").is_empty());
    }

    #[test]
    fn the_allowlist_names_lines_and_flags_stale_ones() {
        let allow =
            Allow::parse("# fixtures\nfx/a.txt:1,3-4:U+001B terminal colour fixture\nfx/gone.txt:2:U+200B no longer there\n").unwrap();
        let files = vec![
            ("fx/a.txt".to_string(), b"\x1b[31m\nplain\n\x1b[0m\nx\x1b\n\x1b new\n".to_vec()),
            ("src/b.md".to_string(), "x\u{200B}".as_bytes().to_vec()),
            ("bin".to_string(), vec![0, 0x1b]),
            ("logo.png".to_string(), vec![0x1b]),
        ];
        let (left, stale) = check(&files, &allow);
        let left: Vec<String> = left.iter().map(ToString::to_string).collect();
        // Line 5 of fx/a.txt is not listed: a new escape there is a finding.
        assert_eq!(left, ["fx/a.txt:5:1: U+001B control character", "src/b.md:1:2: U+200B zero-width character"]);
        assert_eq!(stale, [("fx/gone.txt".to_string(), 2, 0x200B)]);
        let e = gate(&files, &allow, true, "t").unwrap_err();
        assert!(e.contains("src/b.md:1:2: U+200B") && e.contains("fx/gone.txt:2:U+200B matches nothing"), "{e}");
        assert!(Allow::parse("fx/a.txt:1:U+001B").is_err(), "a reason is required");
        assert!(Allow::parse("fx/a.txt:U+001B reason").is_err(), "lines are required");
        assert!(Allow::parse("fx/a.txt:1:1B reason").is_err());
        assert!(Allow::parse("fx/a.txt:3-1:U+001B reason").is_err());
        let e = Allow::parse("src/a.rs:4:U+202E a test").unwrap_err();
        assert!(e.contains("never allowlisted"), "{e}");
        assert!(Allow::parse("src/a.rs:4:U+001E a test separator").is_ok(), "a control character in code may be");
    }

    #[test]
    fn the_checked_in_allowlist_parses() {
        let a = allowlist(&crate::skill::root()).unwrap();
        assert!(!a.entries.is_empty());
    }

    /// The gate over this workspace's tracked files, so a local `cargo
    /// test` (the dev repository has no CI) runs it too.
    #[test]
    fn the_tracked_tree_passes_the_gate() {
        if let Err(e) = tracked_gate(&crate::skill::root()) {
            panic!("{e}");
        }
    }
}
