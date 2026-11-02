//! `calibrate-baseline`: what agents actually spent on rule and handoff
//! lookups before atlas, from real Claude Code transcripts.
//!
//! The token ledger's counterfactual (`atlas::tokens`) is an upper bound:
//! the bytes a literal grep prints plus every file window a reader would
//! open. `cargo xtask calibrate-baseline` measures the other side: the bytes of
//! the tool results agents read when they looked something up in the files
//! atlas now serves, in transcripts dated before atlas. Every number comes
//! from a fixed rule below; nothing is estimated. The result is saved to
//! `baseline_log`, and `--token-report` heads its output with the measured
//! ratio, the upper bound beside it labelled as such.
//!
//! # Method (all rules are mechanical)
//!
//! 1. Transcripts: every `*.jsonl` below `--transcripts` (main sessions and
//!    `subagents/`), read only. Only tool calls timestamped before
//!    `--before` count, and a transcript stops counting at its first call to
//!    an atlas tool (`mcp__atlas__*`).
//! 2. Lookup targets: the configuration's vault and source roots: a path
//!    below a vault root, a path below a source root that one of its
//!    `include` globs matches, or a file whose name matches the handoff
//!    pattern (default: a Markdown file with "handoff" in its name).
//! 3. Lookup calls:
//!    * search: `Grep` whose `path` is a target or below a target root;
//!      `Bash` or `PowerShell` whose command runs grep, rg, findstr or
//!      Select-String and reads only targets: one of its words (shell-split)
//!      is a target path, absolute or relative to the session folder, or has
//!      the file name an include names exactly, or a handoff file name, and
//!      no word names a file (by its extension) that is not a target. A
//!      compound command that also reads other files is not a lookup.
//!    * read: `Read` of a target file; `Bash` or `PowerShell` whose command
//!      runs cat, sed -n, head, tail, awk, less, type or Get-Content and
//!      reads only targets, by the same rule.
//!
//!    Commands that also write (`sed -i`, a `>` redirect to a file, `git`,
//!    `python`, `py`, `Out-File`, `Set-Content`) are not lookups.
//! 4. Episodes: a search call opens a new episode; a read call joins the
//!    open episode when the previous tool call in the same transcript was a
//!    lookup call, else it opens one. An episode is one question answered.
//! 5. Bytes: the UTF-8 bytes of each lookup call's tool result as the agent
//!    received it (the text parts of the result content).
//! 6. The atlas side, read from the token ledger when reported: answers of
//!    the comparable read tools (`atlas::baseline::COMPARABLE`), their returned bytes, and
//!    their counterfactual (grep + read).
//!
//! Ratios per answer: the **measured** ratio is the median bytes of a
//! pre-atlas episode over the median bytes atlas returned per answer (the
//! means are printed beside it); the **upper bound** is the sum of the
//! counterfactual over the sum returned, over answers that have one.

use std::path::{Path, PathBuf};

use atlas::baseline::{median, quantile, Baseline};
use atlas::json::Value;
use atlas::regex::{Regex, RegexBuilder};

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

fn absolute(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
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
    file_word: Regex,
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
        let file_word = Regex::new(r"^[^*?$]+\.(md|rs|txt|json|toml|py|ps1|sh|log|wgsl|csv|tsv|yaml|yml)$").map_err(|e| e.to_string())?;
        Ok(Targets { roots, names, handoff, file_word })
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
                        if globs.iter().any(|g| atlas::glob::matches(g, rel) || atlas::glob::matches(g, base_name(rel))) {
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
        for w in shell_words(&command.replace('\\', "/")) {
            for part in w.split([';', '|', '&', '<', '>', '(', ')']) {
                if part.is_empty() || part.starts_with('-') {
                    continue;
                }
                let n = norm(part);
                let base = base_name(&n);
                if self.names.iter().any(|x| x == base) || self.handoff.is_match(base) || self.is_target(part, cwd) {
                    hit = true;
                } else if self.file_word.is_match(base) && !n.starts_with("/dev/") {
                    return false;
                }
            }
        }
        hit
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
    if ["sed -i", "out-file", "set-content", "python"].iter().any(|w| l.contains(w)) || has_word(&l, &["git", "py"]) {
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
            (!p.is_empty() && t.is_target(p, cwd)).then_some(Kind::Search)
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

fn result_bytes(c: Option<&Value>) -> usize {
    match c {
        Some(Value::String(s)) => s.len(),
        Some(Value::Array(a)) => a.iter().filter(|x| x.get("type").and_then(Value::as_str) == Some("text")).filter_map(|x| x.get("text").and_then(Value::as_str)).map(str::len).sum(),
        _ => 0,
    }
}

/// The byte totals of the lookup episodes of one transcript.
pub fn scan(path: &Path, before: &str, t: &Targets) -> Vec<usize> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut calls: Vec<(String, Option<Kind>)> = Vec::new();
    let mut results: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut stopped = false;
    for line in text.lines() {
        if !line.contains("\"tool_use\"") && !line.contains("\"tool_result\"") {
            continue;
        }
        let Ok(v) = atlas::json::parse(line) else { continue };
        let ts = v.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("");
        let Some(Value::Array(items)) = v.pointer("/message/content") else { continue };
        for x in items {
            let id = |k: &str| x.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            match x.get("type").and_then(Value::as_str) {
                Some("tool_use") if !stopped => {
                    let name = id("name");
                    if ts >= before || name.starts_with("mcp__atlas__") {
                        stopped = true;
                        continue;
                    }
                    let input = x.get("input").cloned().unwrap_or(Value::Null);
                    calls.push((id("id"), classify(&name, &input, cwd, t)));
                }
                Some("tool_result") => {
                    results.insert(id("tool_use_id"), result_bytes(x.get("content")));
                }
                _ => {}
            }
        }
    }
    let mut episodes: Vec<usize> = Vec::new();
    let mut prev: Option<Kind> = None;
    for (id, kind) in calls {
        let Some(k) = kind else {
            prev = None;
            continue;
        };
        let b = results.get(&id).copied().unwrap_or(0);
        match (k, prev, episodes.last_mut()) {
            (Kind::Read, Some(_), Some(last)) => *last += b,
            _ => episodes.push(b),
        }
        prev = Some(k);
    }
    episodes
}

pub fn transcripts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() && e.path().extension().is_some_and(|x| x == "jsonl") => out.push(e.path()),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Measures every transcript below `dir`, reporting progress on stderr
/// every 50 files.
pub fn measure(dir: &Path, before: &str, t: &Targets) -> Baseline {
    let files = transcripts(dir);
    let mut all: Vec<f64> = Vec::new();
    let mut used = 0;
    for (i, p) in files.iter().enumerate() {
        let e = scan(p, before, t);
        used += usize::from(!e.is_empty());
        all.extend(e.into_iter().map(|b| b as f64));
        if (i + 1) % 50 == 0 || i + 1 == files.len() {
            eprintln!("[{}/{}] transcripts scanned, {} lookup episodes", i + 1, files.len(), all.len());
        }
    }
    all.sort_by(f64::total_cmp);
    let total: f64 = all.iter().sum();
    Baseline {
        before: before.to_string(),
        transcripts: files.len(),
        used,
        episodes: all.len(),
        mean: if all.is_empty() { 0.0 } else { total / all.len() as f64 },
        median: median(&all),
        p90: quantile(&all, 0.9),
        total: total as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> Targets {
        Targets::new(&[PathBuf::from("V:/notes vault")], &[(PathBuf::from("R:/repo/.claude"), vec!["RULES.md".into()]), (PathBuf::from("R:/repo/docs"), vec!["*.md".into()])], DEFAULT_HANDOFF).unwrap()
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
        let c = |cmd: &str| classify("Bash", &atlas::json!({ "command": cmd }), "R:/repo", &t);
        assert_eq!(c("grep -n FOO /r/repo/.claude/RULES.md | head"), Some(Kind::Search));
        assert_eq!(c("cd /r/repo/.claude && sed -n 1,40p RULES.md"), Some(Kind::Read));
        assert_eq!(c("cat \"V:/notes vault/a.md\""), Some(Kind::Read));
        assert_eq!(c("grep -n FOO RULES.md src/main.rs"), None);
        assert_eq!(c("sed -i s/a/b/ RULES.md"), None);
        assert_eq!(c("cat RULES.md > out.md"), None);
        assert_eq!(c("grep -n x RULES.md 2>/dev/null"), Some(Kind::Search));
        assert_eq!(c("git log RULES.md"), None);
        assert_eq!(c("ls docs"), None);
        let g = classify("Grep", &atlas::json!({"pattern": "x", "path": "R:/repo/docs"}), "", &t);
        assert_eq!(g, Some(Kind::Search));
        assert_eq!(classify("Read", &atlas::json!({"file_path": "R:/repo/src/a.rs"}), "", &t), None);
    }

    #[test]
    fn shell_words_honour_quotes() {
        assert_eq!(shell_words(r#"cat "a b.md" 'c d' e\ f"#), vec!["cat", "a b.md", "c d", "e f"]);
        assert_eq!(shell_words("a ''"), vec!["a", ""]);
    }

    #[test]
    fn episodes_group_reads_after_a_search() {
        let dir = std::env::temp_dir().join(format!("atlas-baseline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let use_ = |id: &str, name: &str, input: Value, ts: &str| atlas::json!({"type": "assistant", "timestamp": ts, "cwd": "R:/repo", "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}}).to_string();
        let res = |id: &str, n: usize| atlas::json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "x".repeat(n)}]}}).to_string();
        let lines = [
            use_("1", "Grep", atlas::json!({"pattern": "a", "path": "R:/repo/docs"}), "2026-01-01T00:00:00Z"),
            res("1", 100),
            use_("2", "Read", atlas::json!({"file_path": "R:/repo/docs/a.md"}), "2026-01-01T00:00:01Z"),
            res("2", 1000),
            use_("3", "Bash", atlas::json!({"command": "cargo build"}), "2026-01-01T00:00:02Z"),
            res("3", 5),
            use_("4", "Read", atlas::json!({"file_path": "R:/repo/.claude/RULES.md"}), "2026-01-01T00:00:03Z"),
            res("4", 40),
            use_("5", "mcp__atlas__recall", atlas::json!({}), "2026-01-01T00:00:04Z"),
            use_("6", "Read", atlas::json!({"file_path": "R:/repo/.claude/RULES.md"}), "2026-01-01T00:00:05Z"),
            res("6", 7),
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        assert_eq!(scan(&p, "2027", &targets()), vec![1100, 40]);
        assert_eq!(scan(&p, "2026-01-01T00:00:01Z", &targets()), vec![100]);
        let b = measure(&dir, "2027", &targets());
        assert_eq!((b.episodes, b.median, b.total), (2, 570.0, 1140));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
