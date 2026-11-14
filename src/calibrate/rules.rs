//! The lookup rules: which pre-atlas tool calls were rule and handoff
//! lookups, and how they group into episodes (see [`super`] for the method).
//!
//! Everything that turns a transcript's tool calls into the measured
//! numbers is in this file, and its text (comments and blank lines
//! removed) is hashed by a test against [`super::RULES_HISTORY`]: a change
//! here that is not recorded there with a new [`super::RULES_VERSION`]
//! fails the build's tests, and a new version recalibrates every store.

use std::path::PathBuf;

use crate::json::Value;
use crate::regex::{Regex, RegexBuilder};

/// The default handoff file pattern.
pub const DEFAULT_HANDOFF: &str = r"handoff[^/]*\.md$";

fn norm(p: &str) -> String {
    let mut s = p.trim().trim_matches(['"', '\'']).replace('\\', "/").to_lowercase();
    // `/h/x` and `/mnt/h/x` are `h:/x`.
    let b = s.as_bytes();
    let drive = |rest: &str| -> Option<String> {
        let r = rest.as_bytes();
        (r.len() >= 2 && r[0].is_ascii_alphabetic() && r[1] == b'/').then(|| format!("{}:/{}", r[0] as char, &rest[2..]))
    };
    if b.first() == Some(&b'/') {
        if let Some(d) = s.strip_prefix("/mnt/").and_then(drive).or_else(|| drive(&s[1..])) {
            s = d;
        } else if s.len() == 2 && b[1].is_ascii_alphabetic() {
            s = format!("{}:", b[1] as char);
        }
    }
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    s.trim_end_matches('/').to_string()
}

/// Whether a normalised path is absolute: a drive (`h:`) or `/`-rooted
/// (Unix; `/h/x` and `/mnt/h/x` are drives already).
fn absolute(p: &str) -> bool {
    let b = p.as_bytes();
    b.first() == Some(&b'/') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// Whether a normalised word names a path outside the cwd: a drive, or a
/// `/`-rooted word that looks like a path, not a sed address or a grep
/// pattern (`/^## next/,/^## /p`, `/api`): two or more components and
/// none of `^ $ * ? [ ] { } , + \` or whitespace.
fn path_word(p: &str) -> bool {
    match p.strip_prefix('/') {
        Some(r) => r.contains('/') && !r.contains(|c: char| c.is_whitespace() || "^$*?[]{},+\\".contains(c)),
        None => absolute(p),
    }
}

fn base_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// The files atlas serves, as lookup targets.
pub struct Targets {
    /// Normalised root, and its include globs (`None`: everything below).
    roots: Vec<(String, Option<Vec<String>>)>,
    /// File names an include names exactly (no wildcard).
    names: Vec<String>,
    handoff: Regex,
}

/// Extensions a shell word names a file by.
const FILE_EXTS: &[&str] = &["md", "rs", "txt", "json", "toml", "py", "ps1", "sh", "log", "wgsl", "csv", "tsv", "yaml", "yml"];

/// Whether a shell word is a file name (was
/// `^[^*?$]+\.(md|rs|txt|json|toml|py|ps1|sh|log|wgsl|csv|tsv|yaml|yml)$`):
/// no wildcard or `$`, something before the dot, a known extension.
fn file_word(s: &str) -> bool {
    !s.contains(['*', '?', '$'])
        && FILE_EXTS.iter().any(|e| s.len() > e.len() + 1 && s.ends_with(e) && s.as_bytes()[s.len() - e.len() - 1] == b'.')
}

impl Targets {
    /// Targets from vault folders, source roots with their include globs,
    /// and a handoff file-name pattern.
    ///
    /// # Errors
    ///
    /// A handoff pattern the regex engine refuses.
    pub fn new(vaults: &[PathBuf], sources: &[(PathBuf, Vec<String>)], handoff: &str) -> std::result::Result<Targets, String> {
        let mut roots: Vec<(String, Option<Vec<String>>)> = vaults.iter().map(|v| (norm(&v.to_string_lossy()), None)).collect();
        roots.extend(sources.iter().map(|(r, inc)| (norm(&r.to_string_lossy()), Some(inc.iter().map(|g| g.to_lowercase()).collect()))));
        let mut names: Vec<String> = roots.iter().filter_map(|(_, i)| i.as_ref()).flatten().filter(|g| !g.contains(['*', '?', '['])).cloned().collect();
        names.sort();
        names.dedup();
        let handoff = RegexBuilder::new(handoff).case_insensitive(true).build().map_err(|e| format!("handoff pattern: {e}"))?;
        Ok(Targets { roots, names, handoff })
    }

    /// Whether a path (absolute, or relative to `cwd`) is a target.
    pub fn is_target(&self, path: &str, cwd: &str) -> bool {
        let mut p = norm(path);
        if !absolute(&p) && !cwd.is_empty() {
            p = norm(&format!("{cwd}/{p}"));
        }
        if self.handoff.is_match(base_name(&p)) {
            return true;
        }
        for (root, inc) in &self.roots {
            if p == *root {
                return true;
            }
            if let Some(rel) = p.strip_prefix(root.as_str()).and_then(|r| r.strip_prefix('/')) {
                match inc {
                    None => return true,
                    Some(globs) => {
                        if globs.iter().any(|g| crate::glob::matches(g, rel) || crate::glob::matches(g, base_name(rel))) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Whether a shell command reads only targets (see the module docs).
    pub fn names_target(&self, command: &str, cwd: &str) -> bool {
        let mut hit = false;
        // Whether the previous word was `cd`: only its target may be a
        // folder above a root.
        let mut after_cd = false;
        for w in shell_words(&command.replace('\\', "/")) {
            for part in w.split([';', '|', '&', '<', '>', '(', ')']) {
                // A flag (`cd -P DIR`) keeps `after_cd` for the word after it.
                if part.is_empty() || part.starts_with('-') {
                    continue;
                }
                let cd_target = std::mem::replace(&mut after_cd, part.eq_ignore_ascii_case("cd"));
                // `NAME=VALUE` (a variable set for later words): its value.
                let part = match part.split_once('=') {
                    Some((k, v)) if !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => v,
                    _ => part,
                };
                if part.is_empty() {
                    continue;
                }
                let n = norm(part);
                let base = base_name(&n);
                // A source root with include globs named as a folder (`cd ROOT`,
                // `grep -r x .`) says nothing about which files are read.
                if self.is_globbed_root(part, cwd) {
                    continue;
                }
                if self.names.iter().any(|x| x == base) || self.handoff.is_match(base) || self.is_target(part, cwd) {
                    hit = true;
                } else if !n.starts_with("/dev/")
                    && (file_word(base)
                        // An absolute path outside every target (a scratch
                        // folder, a build log), or a path through a
                        // variable (unknown): not only targets.
                        || (path_word(&n) && !(cd_target && self.holds_a_root(&n)))
                        || (n.starts_with('$') && n.contains('/')))
                {
                    return false;
                }
            }
        }
        hit
    }

    /// Whether a path (absolute, or relative to `cwd`) is a source root that
    /// has include globs.
    pub fn is_globbed_root(&self, path: &str, cwd: &str) -> bool {
        let mut p = norm(path);
        if !absolute(&p) && !cwd.is_empty() {
            p = norm(&format!("{cwd}/{p}"));
        }
        self.roots.iter().any(|(r, inc)| inc.is_some() && *r == p)
    }

    /// Whether a normalised absolute path is a folder above a target root
    /// (`cd R:/work && grep ... repo/docs/a.md`); asked of a `cd` target
    /// only, since any other word naming such a folder reads all below it.
    fn holds_a_root(&self, p: &str) -> bool {
        self.roots.iter().any(|(r, _)| r.strip_prefix(p).is_some_and(|rest| rest.starts_with('/')))
    }

    /// The roots and their include globs, one per line, for a fingerprint.
    pub fn describe(&self) -> String {
        self.roots
            .iter()
            .map(|(r, inc)| match inc {
                None => format!("vault {r}
"),
                Some(g) => format!("source {r} {}
", g.join(",")),
            })
            .collect()
    }
}

/// POSIX-style word splitting: whitespace separates words; single quotes
/// take everything literally; double quotes and backslashes outside single
/// quotes escape the next character (paths arrive with forward slashes).
pub fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut any = false;
    let mut chars = s.chars();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => {
                single = !single;
                any = true;
            }
            '"' if !single => {
                double = !double;
                any = true;
            }
            '\\' if !single => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !single && !double => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn has_word(hay: &str, words: &[&str]) -> bool {
    // A word preceded by the start, whitespace or `|;&(` and followed by
    // whitespace or the end.
    let lower = hay.to_lowercase();
    for w in words {
        let mut from = 0;
        while let Some(i) = lower[from..].find(w) {
            let at = from + i;
            let before = lower[..at].chars().next_back();
            let after = lower[at + w.len()..].chars().next();
            if before.is_none_or(|c| c.is_whitespace() || "|;&(".contains(c)) && after.is_none_or(char::is_whitespace) {
                return true;
            }
            from = at + w.len();
        }
    }
    false
}

/// Whether a command also writes (see the module docs).
fn writes(cmd: &str) -> bool {
    let l = cmd.to_lowercase();
    if ["sed -i", "out-file", "set-content", "python", "perl -pi", "perl -i"].iter().any(|w| l.contains(w)) || has_word(&l, &["git", "py", "tee", "mv", "cp", "rm"]) {
        return true;
    }
    // A `>` redirect to a file: not `2>`, `&>`, `>&`.
    let b = l.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c != b'>' || (i > 0 && (b[i - 1].is_ascii_digit() || b[i - 1] == b'&')) {
            continue;
        }
        let rest = l[i + 1..].trim_start_matches('>').trim_start();
        if rest.chars().next().is_some_and(|n| n != '&' && !n.is_whitespace()) {
            return true;
        }
    }
    false
}

/// A lookup call's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A search (grep and the like).
    Search,
    /// A read.
    Read,
}

/// Whether a tool call is a lookup, and which kind (see the module docs).
pub fn classify(name: &str, input: &Value, cwd: &str, t: &Targets) -> Option<Kind> {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match name {
        "Grep" => {
            let p = if s("path").is_empty() { cwd } else { s("path") };
            // A globbed source root searched whole is a code search unless the
            // search is limited to Markdown.
            let md_only = md_glob(s("glob")) || s("type") == "md";
            (!p.is_empty() && t.is_target(p, cwd) && (md_only || !t.is_globbed_root(p, cwd))).then_some(Kind::Search)
        }
        "Read" => (!s("file_path").is_empty() && t.is_target(s("file_path"), cwd)).then_some(Kind::Read),
        "Bash" | "PowerShell" => {
            let cmd = s("command");
            if cmd.is_empty() || writes(cmd) || !t.names_target(cmd, cwd) {
                None
            } else if has_word(cmd, &["grep", "rg", "findstr", "select-string", "sls"]) {
                Some(Kind::Search)
            } else if has_word(cmd, &["cat", "head", "tail", "awk", "less", "type", "get-content", "gc"]) || cmd.to_lowercase().contains("sed -n") {
                Some(Kind::Read)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether a Grep glob matches only Markdown files: every alternative of its
/// braces (`*.{md,txt}` is `*.md` and `*.txt`) ends in `.md`, so `*.mdx` and
/// `*.{md,rs}` do not.
fn md_glob(glob: &str) -> bool {
    let g = glob.to_ascii_lowercase();
    match (g.find('{'), g.rfind('}')) {
        (Some(a), Some(b)) if a < b => {
            let (head, tail) = (&g[..a], &g[b + 1..]);
            g[a + 1..b].split(',').all(|alt| md_glob(&format!("{head}{alt}{tail}")))
        }
        _ => !g.contains(['{', '}', ',']) && g.ends_with(".md"),
    }
}

/// The UTF-8 bytes of a tool result as the agent received it (its text parts).
pub fn result_bytes(c: Option<&Value>) -> usize {
    match c {
        Some(Value::String(s)) => s.len(),
        Some(Value::Array(a)) => a.iter().filter(|x| x.get("type").and_then(Value::as_str) == Some("text")).filter_map(|x| x.get("text").and_then(Value::as_str)).map(str::len).sum(),
        _ => 0,
    }
}

/// Whether a tool is atlas's, or its ancestor's (the vault server served
/// the same vaults before atlas): a transcript stops counting at the first.
pub fn is_atlas_tool(name: &str) -> bool {
    ["mcp__atlas__", "mcp__plugin_atlas_", "mcp__vault__"].iter().any(|p| name.starts_with(p))
}

/// One pre-atlas lookup episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    /// The bytes the agent read.
    pub bytes: usize,
    /// The first call's timestamp.
    pub time: String,
    /// Each call: the tool and its target, for an audit.
    pub calls: Vec<String>,
}

/// A lookup call as one audit line: the tool and what it read.
pub fn call_line(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let what = match name {
        "Grep" => format!("{} in {}", s("pattern"), s("path")),
        "Read" => s("file_path").to_string(),
        _ => s("command").to_string(),
    };
    let what: String = what.replace(['\n', '\t'], " ").chars().take(400).collect();
    format!("{name}: {what}")
}


/// One tool call before the cutoff, as the rules read it: what a
/// calibration stores, so that new rules or targets apply to it after
/// Claude Code has deleted the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The `tool_use` id.
    pub id: String,
    /// When it was made.
    pub time: String,
    /// The session folder of its line.
    pub cwd: String,
    /// The tool.
    pub name: String,
    /// The input keys the rules read ([`kept_input`]).
    pub input: Value,
    /// The bytes of its result ([`result_bytes`]); 0 for a tool whose input
    /// is not kept.
    pub bytes: usize,
}

/// The input keys the rules and the audit line read, by tool; every other
/// tool's input is not read (its call only closes an episode).
pub fn kept_keys(name: &str) -> &'static [&'static str] {
    match name {
        "Grep" => &["pattern", "path", "glob", "type"],
        "Read" => &["file_path"],
        "Bash" | "PowerShell" => &["command"],
        _ => &[],
    }
}

/// A call's input cut to [`kept_keys`] (`null` when none are kept).
pub fn kept_input(name: &str, input: &Value) -> Value {
    let keys = kept_keys(name);
    if keys.is_empty() {
        return Value::Null;
    }
    Value::Object(keys.iter().filter_map(|k| input.get(k).map(|v| (k.to_string(), v.clone()))).collect())
}

/// The lookup episodes of one transcript's calls, in order (see the module
/// docs of [`super`]). With `seen`, a call whose id is already in it (a
/// replay's or a resumed session's copy of a call another transcript
/// holds) counts nothing and closes the open episode; every id is added.
pub fn episodes(calls: &[Call], t: &Targets, mut seen: Option<&mut std::collections::HashSet<String>>) -> Vec<Episode> {
    let mut out: Vec<Episode> = Vec::new();
    let mut prev: Option<Kind> = None;
    for c in calls {
        let copy = match seen.as_deref_mut() {
            Some(s) if !c.id.is_empty() => !s.insert(c.id.clone()),
            _ => false,
        };
        let kind = if copy { None } else { classify(&c.name, &c.input, &c.cwd, t) };
        let Some(k) = kind else {
            prev = None;
            continue;
        };
        let line = format!("{} ({} B)", call_line(&c.name, &c.input), c.bytes);
        match (k, prev, out.last_mut()) {
            (Kind::Read, Some(_), Some(last)) => {
                last.bytes += c.bytes;
                last.calls.push(line);
            }
            _ => out.push(Episode { bytes: c.bytes, time: c.time.clone(), calls: vec![line] }),
        }
        prev = Some(k);
    }
    out
}

/// The measured numbers over every episode's bytes.
pub fn summarize(before: &str, transcripts: usize, used: usize, mut all: Vec<f64>) -> crate::baseline::Baseline {
    all.sort_by(f64::total_cmp);
    let total: f64 = all.iter().sum();
    crate::baseline::Baseline {
        before: before.to_string(),
        transcripts,
        used,
        episodes: all.len(),
        mean: if all.is_empty() { 0.0 } else { total / all.len() as f64 },
        median: crate::baseline::median(&all),
        p90: crate::baseline::quantile(&all, 0.9),
        total: total as u64,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inputs for [`file_word`]: random words from its pieces, and edge
    /// cases (empty, a bare extension, wildcards, Unicode, line endings).
    fn file_words() -> Vec<String> {
        const PIECES: &[&str] =
            &[".", "md", "rs", "yaml", "yml", "ps1", "a", "/", "*", "?", "$", "-", " ", "\n", "\r\n", "\u{e9}", "\u{663}", "x.md", ".json"];
        let mut out: Vec<String> = ["", ".md", "a.md", "a.mdx", "*.md", "$x.rs", "a.b.toml", ".", "md", "\u{e9}.rs", "a\n.md", "a.md\n"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for i in 0..20_000 {
            let mut s = String::new();
            for _ in 0..(1 + i % 9) {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                s.push_str(PIECES[(x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % PIECES.len()]);
            }
            out.push(s);
        }
        out
    }

    #[test]
    fn file_word_matches_its_old_pattern() {
        let re = Regex::new(r"^[^*?$]+\.(md|rs|txt|json|toml|py|ps1|sh|log|wgsl|csv|tsv|yaml|yml)$").unwrap();
        let words = file_words();
        let hits = words.iter().filter(|w| file_word(w)).count();
        assert!(hits > 500, "only {hits} words match");
        for w in &words {
            assert_eq!(file_word(w), re.is_match(w), "{w:?}");
        }
    }

    #[test]
    #[ignore = "timing: run in release with --nocapture"]
    fn file_word_timing() {
        let re = Regex::new(r"^[^*?$]+\.(md|rs|txt|json|toml|py|ps1|sh|log|wgsl|csv|tsv|yaml|yml)$").unwrap();
        let words = file_words();
        let time = |f: &dyn Fn(&str) -> bool| {
            (0..5)
                .map(|_| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(words.iter().filter(|w| f(w)).count());
                    t.elapsed().as_secs_f64()
                })
                .fold(f64::MAX, f64::min)
        };
        let (a, b) = (time(&|w| re.is_match(w)), time(&|w| file_word(w)));
        println!("| calibrate file word | {:.2} ms | {:.2} ms | {:.1}x |", a * 1e3, b * 1e3, a / b);
    }

    fn targets() -> Targets {
        Targets::new(
            &[PathBuf::from("V:/notes vault")],
            &[(PathBuf::from("R:/repo/.claude"), vec!["RULES.md".into()]), (PathBuf::from("R:/repo/docs"), vec!["*.md".into()])],
            DEFAULT_HANDOFF,
        )
        .unwrap()
    }

    #[test]
    fn targets_follow_roots_includes_and_handoffs() {
        let t = targets();
        assert!(t.is_target("R:\\repo\\.claude\\RULES.md", ""));
        assert!(t.is_target("/r/repo/docs/a.md", ""));
        assert!(t.is_target("docs/a.md", "R:/repo"));
        assert!(!t.is_target("R:/repo/.claude/other.md", ""));
        assert!(t.is_target("V:/notes vault/x/y.md", ""));
        assert!(t.is_target("Z:/w/LANE_HANDOFF.md", ""));
        assert!(!t.is_target("R:/repo/src/main.rs", ""));
    }

    #[test]
    fn shell_commands_count_only_when_they_read_only_targets() {
        let t = targets();
        let c = |cmd: &str| classify("Bash", &crate::json!({ "command": cmd }), "R:/repo", &t);
        assert_eq!(c("grep -n FOO /r/repo/.claude/RULES.md | head"), Some(Kind::Search));
        assert_eq!(c("cd /r/repo/.claude && sed -n 1,40p RULES.md"), Some(Kind::Read));
        assert_eq!(c("cat \"V:/notes vault/a.md\""), Some(Kind::Read));
        assert_eq!(c("grep -n FOO RULES.md src/main.rs"), None);
        assert_eq!(c("sed -i s/a/b/ RULES.md"), None);
        assert_eq!(c("cat RULES.md > out.md"), None);
        assert_eq!(c("grep -n x RULES.md 2>/dev/null"), Some(Kind::Search));
        assert_eq!(c("git log RULES.md"), None);
        assert_eq!(c("ls docs"), None);
        // A scratch path, set in a variable or named outright, is not a target.
        assert_eq!(c("S=/c/tmp/scratch; grep -n x $S/notes RULES.md"), None);
        assert_eq!(c("grep -n x /c/tmp/scratch/out RULES.md"), None);
        // A globbed source root named as a folder says nothing; code is not a target.
        assert_eq!(c("cd /r/repo/docs && grep -rn x --include=*.rs src"), None);
        assert_eq!(c("perl -pi -e s/a/b/ RULES.md"), None);
        // A folder above a target root, as a `cd` target, is not a read of
        // anything else; named to grep, it is a search of all below it.
        assert_eq!(c("cd /r && grep -n x repo/.claude/RULES.md"), Some(Kind::Search));
        assert_eq!(c("CD /r; grep -n x RULES.md"), Some(Kind::Search));
        assert_eq!(c("cd -P /r && grep -n x RULES.md"), Some(Kind::Search));
        assert_eq!(c("grep -rn x /r RULES.md"), None);
        assert_eq!(c("cd /r/repo/.claude && grep -rn x /r RULES.md"), None);
        // A `/`-led sed address or grep pattern is not a path.
        assert_eq!(c("sed -n '/^## Next/,/^## /p' notes/handoff.md"), Some(Kind::Read));
        assert_eq!(c("grep -n \"/api\" docs/a.md"), Some(Kind::Search));
        let g = classify("Grep", &crate::json!({"pattern": "x", "path": "R:/repo/docs"}), "", &t);
        assert_eq!(g, None, "a globbed root searched whole");
        let grep = |glob: &str| classify("Grep", &crate::json!({"pattern": "x", "path": "R:/repo/docs", "glob": glob}), "", &t);
        for g in ["*.md", "**/*.MD", "*.{md}", "{a,b}*.md"] {
            assert_eq!(grep(g), Some(Kind::Search), "{g}");
        }
        for g in ["*.mdx", "*.{md,rs}", "*.{md,txt}", "*.md,*.rs", "*.{md"] {
            assert_eq!(grep(g), None, "{g}");
        }
        assert_eq!(classify("Read", &crate::json!({"file_path": "R:/repo/src/a.rs"}), "", &t), None);
    }

    /// Unix paths: `/`-rooted, no drive. Every OS runs this, so a check
    /// that only knows drive letters fails on Windows too.
    #[test]
    fn unix_paths_are_absolute() {
        let t = Targets::new(&[PathBuf::from("/home/u/work/notes")], &[(PathBuf::from("/home/u/work/repo/docs"), vec!["*.md".into()])], DEFAULT_HANDOFF).unwrap();
        assert!(t.is_target("/home/u/work/notes/a.md", "/home/u/work"));
        assert!(t.is_target("/home/u/work/notes/a.md", ""));
        assert!(t.is_target("notes/a.md", "/home/u/work"));
        assert!(t.is_target("/home/u/work/repo/docs/b.md", "/tmp/elsewhere"));
        assert!(!t.is_target("/home/u/work/repo/src/main.rs", "/home/u/work"));
        assert!(t.is_globbed_root("/home/u/work/repo/docs", "/home/u/work"));
        let c = |cmd: &str| classify("Bash", &crate::json!({ "command": cmd }), "/home/u/work", &t);
        assert_eq!(c("cat /home/u/work/notes/a.md"), Some(Kind::Read));
        assert_eq!(c("grep -n x /home/u/work/notes/a.md /tmp/scratch/out"), None);
        assert_eq!(c("sed -n '/^## Next/,/^## /p' notes/handoff.md"), Some(Kind::Read));
        assert_eq!(c("grep -n \"/api\" notes/a.md"), Some(Kind::Search));
        assert_eq!(classify("Read", &crate::json!({"file_path": "/home/u/work/notes/a.md"}), "/home/u/work", &t), Some(Kind::Read));
    }

    #[test]
    fn shell_words_honour_quotes() {
        assert_eq!(shell_words(r#"cat "a b.md" 'c d' e\ f"#), vec!["cat", "a b.md", "c d", "e f"]);
        assert_eq!(shell_words("a ''"), vec!["a", ""]);
    }

}
