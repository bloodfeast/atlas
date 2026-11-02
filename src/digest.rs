//! Digesters: the short summary of a captured output (cargo build and test
//! totals, failures, warnings by lint; grep hits by file; a listing by
//! folder; a source file's outline; or the generic matches and tail) that
//! goes into context in place of the output itself.
//!
//! Every function here is a pure function of its input: the same output
//! always gives the same digest and the same items, so a hook answer is
//! reproducible.
//!
//! The item table ([`items`]) is what `slice --item` and `slice --failures`
//! resolve against. It is recomputed from the stored output on each slice
//! (milliseconds) rather than stored.

use crate::regex::Regex;

/// The digest budget in bytes for a run or a search (`lean.digests.max_bytes`).
pub const DEFAULT_MAX_BYTES: usize = 600;
/// The budget of an outline digest, which lists items by line range.
pub const OUTLINE_MAX_BYTES: usize = 3200;
/// Failures listed by name before "and N more".
pub const MAX_FAILURES: usize = 10;
/// Tail lines a digest keeps verbatim.
pub const TAIL_LINES: usize = 20;
/// The longest line a digest quotes, in characters.
const QUOTE: usize = 160;

/// One named piece of an output: a test, a failure block, a compiler
/// message, a file's hits, an item of a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The name `slice --item` matches (a test path, `error:2`, a file
    /// path, `Type::fn`).
    pub name: String,
    /// First line, 1-based.
    pub first: usize,
    /// Last line, inclusive.
    pub last: usize,
    /// `failed`, `ok`, `ignored`, `error`, `warning`, or the item's kind
    /// (`fn`, `struct`, `heading`, `file`).
    pub status: String,
    /// Nesting depth (outlines).
    pub depth: usize,
}

/// The language a source outline parses, from the file's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Unknown: decided from the text.
    Auto,
    /// Markdown headings.
    Markdown,
    /// Brace-delimited code (Rust, C, C++, Go, JavaScript, TypeScript,
    /// Java, C#, WGSL, ...).
    Brace,
    /// Python.
    Python,
}

/// The language of a file name or of the last path-like word of a command.
pub fn lang_of(name: &str) -> Lang {
    let word = name.split_whitespace().rev().find(|w| w.contains('.')).unwrap_or("");
    let word = word.trim_matches(|c| c == '\'' || c == '"');
    let ext = word.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "mdx" => Lang::Markdown,
        "py" | "pyi" => Lang::Python,
        "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "go" | "js" | "jsx" | "ts" | "tsx" | "java" | "cs" | "kt" | "swift" | "wgsl" | "glsl" | "hlsl" | "zig" | "scala" | "dart" | "php" => Lang::Brace,
        _ => Lang::Auto,
    }
}

/// What an output looks like, which picks its digester.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `cargo test` or `cargo nextest` output.
    CargoTest,
    /// `cargo build`, `check` or `clippy` output.
    CargoBuild,
    /// `path:line:text` hits (grep, rg, the Grep tool's content mode).
    Grep,
    /// One path per line (find, ls -1, Glob, Grep's file list).
    Listing,
    /// A file's text (Read, cat, sed -n, head, Get-Content), outlined as
    /// the language its file name says.
    Source(Lang),
    /// A long check log with a per-package totals table (a test runner's
    /// wrapper): the tables, failures, panics and verdict lines only.
    Summary,
    /// Anything else.
    Generic,
}

fn re(p: &str) -> Regex {
    Regex::new(p).expect("digest pattern")
}

/// The words of a command after leading environment assignments and
/// `cd ... &&` steps: the command that produced the output.
pub fn last_step(cmd: &str) -> &str {
    let mut s = cmd.trim();
    while let Some(i) = s.find("&&") {
        let head = s[..i].trim();
        if head.starts_with("cd ") || head.starts_with("export ") || head.starts_with("set ") {
            s = s[i + 2..].trim();
        } else {
            break;
        }
    }
    loop {
        let first = s.split_whitespace().next().unwrap_or("");
        if first.contains('=') && !first.starts_with('-') && !first.starts_with('=') {
            s = s[first.len()..].trim_start();
        } else {
            break;
        }
    }
    s
}

/// The shape of an output.
///
/// # Arguments
///
/// * `kind` - the handle kind: `read`, `grep`, `glob` (tool results) or
///   `run`, `bash` (commands).
/// * `cmd` - the command, empty for a tool result.
/// * `text` - the output.
pub fn shape(kind: &str, cmd: &str, text: &str) -> Shape {
    match kind {
        "read" => return Shape::Source(lang_of(cmd)),
        "grep" => return Shape::Grep,
        "glob" | "files" => return Shape::Listing,
        _ => {}
    }
    let step = last_step(cmd);
    if text.lines().any(|l| table_head().is_match(l)) {
        return Shape::Summary;
    }
    if re(r"\bcargo\s+(\+\S+\s+)?(test|nextest|t)\b").is_match(cmd) || text.contains("\ntest result: ") {
        return Shape::CargoTest;
    }
    if re(r"\bcargo\s+(\+\S+\s+)?(build|check|clippy|b|c|run|r|doc)\b").is_match(cmd) {
        return Shape::CargoBuild;
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(200).collect();
    if !lines.is_empty() {
        let hit = re(r"^[^\s:][^:]*:\d+[:-]");
        let grep_like = lines.iter().filter(|l| hit.is_match(l)).count();
        if grep_like * 10 >= lines.len() * 6 {
            return Shape::Grep;
        }
    }
    let reader = re(r"^(cat|head|tail|sed|nl|less|more|type|Get-Content|gc)\b");
    if reader.is_match(step) {
        return Shape::Source(lang_of(step));
    }
    if !lines.is_empty() {
        let path = re(r"^\.?[\w@.+~/\\:-]+$");
        let paths = lines.iter().filter(|l| path.is_match(l.trim_end())).count();
        if paths * 10 >= lines.len() * 8 {
            return Shape::Listing;
        }
    }
    Shape::Generic
}

/// The digest of an output: a header line, the shape's summary, the
/// verbatim tail where pass or fail is inferred, and one slice hint line.
///
/// # Arguments
///
/// * `id` - the handle.
/// * `label` - what the header names (the command, or the tool and path).
/// * `shape` - from [`shape`].
/// * `text` - the output.
/// * `exit` - the exit code, when known (`atlas run`); `-1` for "nonzero,
///   code unknown"; `None` makes pass or fail "inferred".
/// * `max_bytes` - the budget for runs and searches; outlines use
///   [`OUTLINE_MAX_BYTES`] scaled the same way.
pub fn digest(id: &str, label: &str, shape: Shape, text: &str, exit: Option<i32>, max_bytes: usize) -> String {
    let n = line_count(text);
    let size = if text.len() >= 10_240 { format!("{} KB", text.len() / 1024) } else { format!("{} B", text.len()) };
    let exit_s = match exit {
        Some(-1) => " exit nonzero".to_string(),
        Some(c) => format!(" exit {c}"),
        None => String::new(),
    };
    let mut out = format!("{id} {}{exit_s}, {n} lines, {size}\n", clip(label, 80));
    let body = match shape {
        Shape::CargoTest => cargo_test(id, text, exit, max_bytes),
        Shape::CargoBuild => cargo_build(id, text, exit, max_bytes),
        Shape::Grep => grep(id, text, max_bytes),
        Shape::Listing => listing(id, text, max_bytes),
        Shape::Source(lang) => source(id, lang, text, max_bytes * OUTLINE_MAX_BYTES / DEFAULT_MAX_BYTES.max(1)),
        Shape::Summary => summary(text) + &hint(id, "--failures | --grep RE"),
        Shape::Generic => generic(id, text, exit, max_bytes),
    };
    out.push_str(&body);
    out
}

/// The items of an output, for `slice --item` and `slice --failures`.
pub fn items(shape: Shape, text: &str) -> Vec<Item> {
    match shape {
        Shape::CargoTest | Shape::Summary => {
            let mut v = test_items(text);
            v.extend(build_items(text));
            v
        }
        Shape::CargoBuild => build_items(text),
        Shape::Grep => grep_items(text),
        Shape::Source(lang) => outline_as(lang, text),
        Shape::Listing | Shape::Generic => Vec::new(),
    }
}

/// Lines in a text, counting a last line without a newline.
pub fn line_count(text: &str) -> usize {
    text.lines().count()
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim_end();
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n).collect();
    t.push_str("...");
    t
}

/// The last lines, verbatim, as many as fit `budget` bytes (at most
/// [`TAIL_LINES`]), with their line numbers.
fn tail(text: &str, budget: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let mut picked: Vec<String> = Vec::new();
    let mut used = 0;
    for (i, l) in lines.iter().enumerate().rev().take(TAIL_LINES) {
        if l.trim().is_empty() && picked.is_empty() {
            continue;
        }
        let line = format!("{}: {}\n", i + 1, clip(l, QUOTE));
        if used + line.len() > budget && !picked.is_empty() {
            break;
        }
        used += line.len();
        picked.push(line);
    }
    picked.reverse();
    let mut s = String::new();
    if !picked.is_empty() {
        s.push_str(&format!("last {} of {n} lines:\n", picked.len()));
        for l in picked {
            s.push_str(&l);
        }
    }
    s
}

fn hint(id: &str, sels: &str) -> String {
    format!("atlas slice {id} {sels}\n")
}

// ---- cargo test ----------------------------------------------------------

/// Totals over every `test result:` line, and the binary count.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Totals {
    passed: u64,
    failed: u64,
    ignored: u64,
    binaries: u64,
}

fn totals(text: &str) -> Totals {
    let r = re(r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored");
    let nx = re(r"^\s*Summary \[.*\] (\d+) tests? run: (\d+) passed(?:, (\d+) failed)?(?:.*?(\d+) skipped)?");
    let mut t = Totals::default();
    for l in text.lines() {
        if let Some(c) = r.captures(l.trim_start()) {
            let g = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<u64>().ok()).unwrap_or(0);
            t.passed += g(1);
            t.failed += g(2);
            t.ignored += g(3);
            t.binaries += 1;
        } else if let Some(c) = nx.captures(l) {
            let g = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<u64>().ok()).unwrap_or(0);
            t.passed += g(2);
            t.failed += g(3);
            t.ignored += g(4);
            t.binaries += 1;
        }
    }
    t
}

fn test_items(text: &str) -> Vec<Item> {
    let lines: Vec<&str> = text.lines().collect();
    let status = re(r"^test (\S+) \.\.\. (ok|FAILED|ignored)");
    let block = re(r"^---- (\S+) (stdout|stderr) ----$");
    let nx = re(r"^\s+(PASS|FAIL|SIGSEGV|SIGABRT|TIMEOUT) \[[^\]]*\] (?:\(\s*\d+/\d+\)\s+)?(\S+) (\S+)\s*$");
    let nxblock = re(r"^--- (STDOUT|STDERR): +(\S+) (\S+) ---$");
    let mut v = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if let Some(c) = status.captures(l) {
            let st = match c.get(2).map(|m| m.as_str()) {
                Some("FAILED") => "failed",
                Some("ignored") => "ignored",
                _ => "ok",
            };
            v.push(Item { name: c[1].to_string(), first: i + 1, last: i + 1, status: st.into(), depth: 0 });
        } else if let Some(c) = nx.captures(l) {
            let st = if &c[1] == "PASS" { "ok" } else { "failed" };
            v.push(Item { name: c[3].to_string(), first: i + 1, last: i + 1, status: st.into(), depth: 0 });
        } else if let Some(c) = block.captures(l).or_else(|| nxblock.captures(l)) {
            let name = if c.get(3).is_some() { c[3].to_string() } else { c[1].to_string() };
            let mut j = i + 1;
            while j < lines.len() {
                let m = lines[j];
                if block.is_match(m) || nxblock.is_match(m) || m.starts_with("failures:") || m.starts_with("test result:") || nx.is_match(m) {
                    break;
                }
                j += 1;
            }
            let mut last = j;
            while last > i + 1 && lines[last - 1].trim().is_empty() {
                last -= 1;
            }
            v.push(Item { name: format!("{name}"), first: i + 1, last, status: "failed-output".into(), depth: 0 });
            i = j;
            continue;
        }
        i += 1;
    }
    v
}

/// The panic location and message of a failure block.
fn panic_of(lines: &[&str]) -> String {
    let new = re(r"panicked at ([^\s]+:\d+):\d+:$");
    let old = re(r"panicked at '(.*)', ([^\s]+:\d+):\d+");
    for (k, l) in lines.iter().enumerate() {
        if let Some(c) = new.captures(l) {
            let mut msg = lines.get(k + 1).map(|m| m.trim().to_string()).unwrap_or_default();
            let lr: Vec<String> = lines[k + 1..]
                .iter()
                .take(6)
                .filter_map(|m| {
                    let t = m.trim();
                    (t.starts_with("left:") || t.starts_with("right:")).then(|| t.to_string())
                })
                .collect();
            if !lr.is_empty() {
                msg = format!("{msg} ({})", lr.join(", "));
            }
            return format!("{} {}", c[1].replace('\\', "/"), clip(&msg, QUOTE));
        }
        if let Some(c) = old.captures(l) {
            return format!("{} {}", c[2].replace('\\', "/"), clip(&c[1], QUOTE));
        }
    }
    String::new()
}

fn cargo_test(id: &str, text: &str, exit: Option<i32>, max: usize) -> String {
    let t = totals(text);
    let items = test_items(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut failed: Vec<&Item> = Vec::new();
    for it in items.iter().filter(|i| i.status == "failed") {
        if !failed.iter().any(|f| f.name == it.name) {
            failed.push(it);
        }
    }
    let mut out = String::new();
    let verdict = match exit {
        Some(0) => "passed",
        Some(_) => "failed",
        None if t.failed > 0 || !failed.is_empty() => "exit status unknown (inferred: failed)",
        None if t.binaries > 0 => "exit status unknown (inferred: passed)",
        None => "exit status unknown (no test result line)",
    };
    out.push_str(&format!("tests {} passed {} failed {} ignored ({} binaries): {verdict}\n", t.passed, t.failed, t.ignored, t.binaries));
    let errors = build_items(text).iter().filter(|i| i.status == "error").count();
    if errors > 0 {
        out.push_str(&format!("build: {errors} errors (atlas slice {id} --item error:1)\n"));
    }
    for f in failed.iter().take(MAX_FAILURES) {
        let block = items.iter().find(|i| i.status == "failed-output" && i.name == f.name);
        let why = block.map(|b| panic_of(&lines[b.first - 1..b.last])).unwrap_or_default();
        out.push_str(&format!("FAILED {} {}\n", f.name, why).replace(" \n", "\n"));
    }
    if failed.len() > MAX_FAILURES {
        out.push_str(&format!("and {} more failed\n", failed.len() - MAX_FAILURES));
    }
    if exit.is_none() || exit != Some(0) {
        // Pass or fail is a guess without an exit code: the verbatim tail
        // keeps a wrong guess from hiding a failure.
        out.push_str(&tail(text, max.saturating_sub(out.len()).max(200)));
    }
    out.push_str(&hint(id, "--failures | --item NAME | --lines A-B | --grep RE"));
    out
}

// ---- cargo build ---------------------------------------------------------

fn build_items(text: &str) -> Vec<Item> {
    let lines: Vec<&str> = text.lines().collect();
    let head = re(r"^(error|warning)(\[[A-Za-z0-9_:]+\])?: (.*)$");
    let skip = re(r"^(error: could not compile|error: aborting|warning: build failed|warning: `[^`]+` \(.*\) generated|warning: unused manifest key|error: test failed|error: \d+ target)");
    let mut v = Vec::new();
    let (mut ne, mut nw) = (0, 0);
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if head.is_match(l) && !skip.is_match(l) {
            let mut j = i + 1;
            loop {
                while j < lines.len() && !lines[j].trim().is_empty() && !head.is_match(lines[j]) {
                    j += 1;
                }
                // A `help:` or `note:` section after a blank line belongs to the message.
                let next = lines.get(j + 1).map_or("", |l| *l);
                if j < lines.len() && lines[j].trim().is_empty() && (next.starts_with("help:") || next.starts_with("note:")) {
                    j += 1;
                    continue;
                }
                break;
            }
            let is_err = l.starts_with("error");
            let name = if is_err {
                ne += 1;
                format!("error:{ne}")
            } else {
                nw += 1;
                format!("warning:{nw}")
            };
            v.push(Item { name, first: i + 1, last: j, status: if is_err { "error".into() } else { "warning".into() }, depth: 0 });
            i = j;
            continue;
        }
        i += 1;
    }
    v
}

fn location(block: &[&str]) -> String {
    let at = re(r"^\s*--> ([^\s]+):(\d+):\d+");
    block.iter().find_map(|l| at.captures(l).map(|c| format!("{}:{}", c[1].replace('\\', "/"), &c[2]))).unwrap_or_default()
}

/// A warning's message with its specifics (backticked names) generalised,
/// so warnings group by lint.
fn lint_key(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut inside = false;
    for c in msg.chars() {
        if c == '`' {
            if !inside {
                out.push('_');
            }
            inside = !inside;
        } else if !inside {
            out.push(c);
        }
    }
    out
}

fn cargo_build(id: &str, text: &str, exit: Option<i32>, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let items = build_items(text);
    let head = re(r"^(error|warning)(\[[A-Za-z0-9_:]+\])?: (.*)$");
    let errors: Vec<&Item> = items.iter().filter(|i| i.status == "error").collect();
    let warnings: Vec<&Item> = items.iter().filter(|i| i.status == "warning").collect();
    let verdict = match exit {
        Some(0) => "ok",
        Some(_) => "failed",
        None if !errors.is_empty() => "exit status unknown (inferred: failed)",
        None if text.contains("Finished ") => "exit status unknown (inferred: ok)",
        None => "exit status unknown",
    };
    let mut out = format!("{} errors, {} warnings: {verdict}\n", errors.len(), warnings.len());
    for e in errors.iter().take(MAX_FAILURES) {
        let b = &lines[e.first - 1..e.last];
        let (code, msg) = head.captures(b[0]).map(|c| (c.get(2).map(|m| m.as_str().to_string()).unwrap_or_default(), c[3].to_string())).unwrap_or_default();
        out.push_str(&format!("{} error{code} {} {}\n", e.name, location(b), clip(&msg, QUOTE)));
    }
    if errors.len() > MAX_FAILURES {
        out.push_str(&format!("and {} more errors\n", errors.len() - MAX_FAILURES));
    }
    if !warnings.is_empty() {
        let mut groups: Vec<(String, usize)> = Vec::new();
        for w in &warnings {
            let msg = head.captures(lines[w.first - 1]).map(|c| lint_key(&c[3])).unwrap_or_default();
            match groups.iter_mut().find(|g| g.0 == msg) {
                Some(g) => g.1 += 1,
                None => groups.push((msg, 1)),
            }
        }
        groups.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let g: Vec<String> = groups.iter().take(6).map(|(m, n)| format!("{n}x {}", clip(m, 60))).collect();
        out.push_str(&format!("warnings: {}\n", g.join("; ")));
    }
    if exit != Some(0) {
        out.push_str(&tail(text, max.saturating_sub(out.len()).max(160)));
    }
    out.push_str(&hint(id, "--item error:N | --item warning:N | --lines A-B | --grep RE"));
    out
}

// ---- grep ----------------------------------------------------------------

fn grep_path(l: &str) -> Option<&str> {
    // `path:line:` or `path-line-` (context lines), path without spaces at
    // its start; Windows drive letters keep their colon.
    let skip = if l.len() > 2 && l.as_bytes()[1] == b':' && l.as_bytes()[0].is_ascii_alphabetic() { 2 } else { 0 };
    let rest = &l[skip..];
    // `path:12:` (a hit), else `path-12-` (a context line).
    for sep in [':', '-'] {
        let mut from = 0;
        while let Some(k) = rest[from..].find(sep) {
            let c = from + k;
            let after = &rest[c + 1..];
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            if c > 0 && digits > 0 && after[digits..].starts_with(sep) {
                return Some(&l[..skip + c]);
            }
            if sep == ':' {
                break;
            }
            from = c + 1;
        }
    }
    None
}

fn grep_items(text: &str) -> Vec<Item> {
    let mut v: Vec<Item> = Vec::new();
    for (i, l) in text.lines().enumerate() {
        let Some(p) = grep_path(l) else { continue };
        match v.last_mut() {
            Some(last) if last.name == p => last.last = i + 1,
            _ => v.push(Item { name: p.to_string(), first: i + 1, last: i + 1, status: "file".into(), depth: 0 }),
        }
    }
    v
}

fn grep(id: &str, text: &str, max: usize) -> String {
    let items = grep_items(text);
    let mut counts: Vec<(String, usize)> = Vec::new();
    for l in text.lines() {
        if let Some(p) = grep_path(l) {
            match counts.iter_mut().find(|c| c.0 == p) {
                Some(c) => c.1 += 1,
                None => counts.push((p.to_string(), 1)),
            }
        }
    }
    let hits: usize = counts.iter().map(|c| c.1).sum();
    let mut out = format!("{hits} hits in {} files\n", counts.len());
    let mut by = counts.clone();
    by.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let top: Vec<String> = by.iter().take(10).map(|(p, n)| format!("{p} {n}")).collect();
    out.push_str(&top.join(", "));
    if by.len() > 10 {
        out.push_str(&format!(", and {} more files", by.len() - 10));
    }
    out.push('\n');
    let budget = max.saturating_sub(out.len() + 80);
    let mut used = 0;
    let mut shown = 0;
    for l in text.lines().filter(|l| grep_path(l).is_some()) {
        let q = format!("{}\n", clip(l, QUOTE));
        if used + q.len() > budget {
            break;
        }
        used += q.len();
        shown += 1;
        out.push_str(&q);
    }
    if shown < hits {
        out.push_str(&format!("({shown} of {hits} hits shown)\n"));
    }
    let _ = items;
    out.push_str(&hint(id, "--item PATH | --grep RE | --lines A-B"));
    out
}

// ---- listing -------------------------------------------------------------

fn listing(id: &str, text: &str, max: usize) -> String {
    let paths: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
    let mut dirs: Vec<(String, usize)> = Vec::new();
    for p in &paths {
        let norm = p.replace('\\', "/");
        let d = match norm.rfind('/') {
            Some(i) => norm[..i + 1].to_string(),
            None => "./".to_string(),
        };
        match dirs.iter_mut().find(|x| x.0 == d) {
            Some(x) => x.1 += 1,
            None => dirs.push((d, 1)),
        }
    }
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut out = format!("{} paths in {} folders\n", paths.len(), dirs.len());
    let top: Vec<String> = dirs.iter().take(10).map(|(d, n)| format!("{d} {n}")).collect();
    out.push_str(&top.join(", "));
    if dirs.len() > 10 {
        out.push_str(&format!(", and {} more folders", dirs.len() - 10));
    }
    out.push('\n');
    let budget = max.saturating_sub(out.len() + 80);
    let mut used = 0;
    let mut shown = 0;
    for p in &paths {
        if used + p.len() + 1 > budget {
            break;
        }
        used += p.len() + 1;
        shown += 1;
        out.push_str(p);
        out.push('\n');
    }
    if shown < paths.len() {
        out.push_str(&format!("({shown} of {} shown)\n", paths.len()));
    }
    out.push_str(&hint(id, "--grep RE | --lines A-B"));
    out
}

// ---- source outline ------------------------------------------------------

/// The outline of a file's text: Markdown headings with their sections,
/// or code items (Rust, C-like, Python) with their end lines, nested by
/// container. Deterministic; a best effort over the text alone.
pub fn outline(text: &str) -> Vec<Item> {
    outline_as(Lang::Auto, text)
}

/// The outline of a text in a known language (see [`outline`]).
pub fn outline_as(lang: Lang, text: &str) -> Vec<Item> {
    let lines: Vec<&str> = text.lines().collect();
    let heading = re(r"^(#{1,6}) +(\S.*?)\s*#*\s*$");
    let py = re(r"^(\s*)(async\s+def|def|class)\s+(\w+)");
    match lang {
        Lang::Markdown => return md_outline(&lines, &heading),
        Lang::Python => return py_outline(&lines, &py),
        Lang::Brace => return code_outline(&lines),
        Lang::Auto => {}
    }
    let headings = lines.iter().filter(|l| heading.is_match(l)).count();
    if headings >= 2 && headings * 4 >= lines.iter().filter(|l| l.starts_with("fn ") || l.starts_with("pub ")).count() {
        return md_outline(&lines, &heading);
    }
    let pys = lines.iter().filter(|l| py.is_match(l)).count();
    let braces = text.bytes().filter(|b| *b == b'{').count();
    if pys > 0 && braces < pys {
        return py_outline(&lines, &py);
    }
    code_outline(&lines)
}

fn md_outline(lines: &[&str], heading: &Regex) -> Vec<Item> {
    let mut v: Vec<Item> = Vec::new();
    let mut fence = false;
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("```") {
            fence = !fence;
        }
        if fence {
            continue;
        }
        if let Some(c) = heading.captures(l) {
            v.push(Item { name: c[2].to_string(), first: i + 1, last: lines.len(), status: "heading".into(), depth: c[1].len() - 1 });
        }
    }
    for k in 0..v.len() {
        let d = v[k].depth;
        if let Some(next) = v[k + 1..].iter().find(|n| n.depth <= d) {
            v[k].last = next.first - 1;
        }
    }
    v
}

fn py_outline(lines: &[&str], py: &Regex) -> Vec<Item> {
    let mut v: Vec<Item> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(c) = py.captures(l) else { continue };
        let indent = c[1].len();
        while stack.last().is_some_and(|s| s.0 >= indent) {
            stack.pop();
        }
        let kind = if &c[2] == "class" { "class" } else { "def" };
        let name = match stack.last() {
            Some((_, p)) => format!("{p}.{}", &c[3]),
            None => c[3].to_string(),
        };
        let mut last = i + 1;
        for (j, m) in lines.iter().enumerate().skip(i + 1) {
            if m.trim().is_empty() {
                continue;
            }
            if m.len() - m.trim_start().len() <= indent {
                break;
            }
            last = j + 1;
        }
        v.push(Item { name: name.clone(), first: i + 1, last, status: kind.into(), depth: stack.len() });
        if kind == "class" {
            stack.push((indent, name));
        }
    }
    v
}

/// The 1-based last line of the brace-delimited item that starts on line
/// index `start` (0-based): see [`brace_end`]. Used for `symbol` end lines.
pub fn item_end(lines: &[&str], start: usize) -> usize {
    brace_end(lines, start)
}

/// The line where the item starting at `start` ends: the line that closes
/// its first brace, the line of a `;` or `,` reached before any brace (a
/// field or a variant), or the line before a `}` that closes its container.
/// Strings, chars and comments are skipped roughly.
fn brace_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i64;
    let mut opened = false;
    let mut block_comment = false;
    // Strings run across lines; a raw string ends at `"` and its hashes.
    let mut in_str = false;
    let mut raw: Option<usize> = None;
    // Parentheses, brackets and (before the body) generics: a comma inside
    // them does not end a field.
    let mut nest = 0i64;
    for (i, l) in lines.iter().enumerate().skip(start) {
        let b = l.as_bytes();
        let mut k = 0;
        while k < b.len() {
            let c = b[k];
            if let Some(n) = raw {
                if c == b'"' && b.len() >= k + 1 + n && b[k + 1..k + 1 + n].iter().all(|h| *h == b'#') {
                    raw = None;
                    k += n;
                }
            } else if block_comment {
                if c == b'*' && b.get(k + 1) == Some(&b'/') {
                    block_comment = false;
                    k += 1;
                }
            } else if in_str {
                if c == b'\\' {
                    k += 1;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'/' if b.get(k + 1) == Some(&b'/') => break,
                    b'/' if b.get(k + 1) == Some(&b'*') => {
                        block_comment = true;
                        k += 1;
                    }
                    b'"' => in_str = true,
                    b'r' if (k == 0 || !(b[k - 1].is_ascii_alphanumeric() || b[k - 1] == b'_')) && {
                        let n = b[k + 1..].iter().take_while(|h| **h == b'#').count();
                        b.get(k + 1 + n) == Some(&b'"')
                    } =>
                    {
                        let n = b[k + 1..].iter().take_while(|h| **h == b'#').count();
                        raw = Some(n);
                        k += 1 + n;
                    }
                    // A char literal like '{' (but not a lifetime 'a).
                    b'\'' if b.get(k + 2) == Some(&b'\'') => k += 2,
                    b'\'' if b.get(k + 1) == Some(&b'\\') => {
                        while k + 1 < b.len() && b[k + 1] != b'\'' {
                            k += 1;
                        }
                        k += 1;
                    }
                    b'{' => {
                        depth += 1;
                        opened = true;
                    }
                    b'}' => {
                        depth -= 1;
                        if opened && depth <= 0 {
                            return i + 1;
                        }
                        if !opened && depth < 0 {
                            // The container closed: the item ended before it.
                            return i.max(start + 1);
                        }
                    }
                    b';' if !opened && depth == 0 => return i + 1,
                    b'(' | b'[' => nest += 1,
                    b')' | b']' => nest -= 1,
                    b'<' if !opened => nest += 1,
                    b'>' if !opened && k > 0 && b[k - 1] != b'-' && b[k - 1] != b'=' => nest -= 1,
                    b',' if !opened && depth == 0 && nest <= 0 => return i + 1,
                    _ => {}
                }
            }
            k += 1;
        }
    }
    lines.len()
}

fn code_outline(lines: &[&str]) -> Vec<Item> {
    let decl = re(
        r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|default|extern\s+\S+)\s+)*(fn|struct|enum|trait|impl|mod|type|const|static|union|macro_rules!|class|interface|function|namespace)\b\s*(.*)$",
    );
    let ident = re(r"^(?:<[^>]*>\s*)?([A-Za-z_][A-Za-z0-9_]*)");
    let mut v: Vec<Item> = Vec::new();
    // Open containers: (end line, indent, name).
    let mut stack: Vec<(usize, usize, String)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(c) = decl.captures(l) else { continue };
        let indent = c[1].len();
        if indent > 8 {
            continue;
        }
        while stack.last().is_some_and(|s| s.0 < i + 1) {
            stack.pop();
        }
        let kind = c[2].trim_end_matches('!').to_string();
        let rest = c.get(3).map_or("", |m| m.as_str());
        let name = if kind == "impl" {
            // `impl<T> Trait for Type {` names the impl by its type.
            let r = rest.trim_start();
            let r = if r.starts_with('<') { r.find('>').map_or(r, |k| &r[k + 1..]) } else { r };
            let r = r.split(" where").next().unwrap_or(r).trim_end_matches('{').trim();
            let ty = r.rsplit(" for ").next().unwrap_or(r).trim();
            ty.split(['<', ' ']).next().unwrap_or(ty).to_string()
        } else {
            ident.captures(rest.trim_start()).map(|m| m[1].to_string()).unwrap_or_default()
        };
        if name.is_empty() {
            continue;
        }
        let end = brace_end(lines, i);
        let full = match stack.last() {
            Some((_, _, p)) => format!("{p}::{name}"),
            None => name.clone(),
        };
        let label = if kind == "impl" { rest.trim_end_matches('{').trim().to_string() } else { full.clone() };
        let _ = label;
        v.push(Item { name: full.clone(), first: i + 1, last: end, status: kind.clone(), depth: stack.len() });
        if matches!(kind.as_str(), "impl" | "mod" | "trait" | "class" | "namespace" | "interface") && end > i + 1 {
            stack.push((end, indent, full));
        }
    }
    v
}

fn source(id: &str, lang: Lang, text: &str, max: usize) -> String {
    let items = outline_as(lang, text);
    let mut out = String::new();
    if items.is_empty() {
        out.push_str("no outline (not code or Markdown); first lines:\n");
        let mut used = 0;
        for (i, l) in text.lines().enumerate().take(15) {
            let q = format!("{}: {}\n", i + 1, clip(l, QUOTE));
            if used + q.len() > max {
                break;
            }
            used += q.len();
            out.push_str(&q);
        }
        out.push_str(&hint(id, "--lines A-B | --grep RE"));
        return out;
    }
    out.push_str(&format!("outline, {} items (the text is not shown; slice it):\n", items.len()));
    let row = |it: &Item| {
        let range = if it.first == it.last { format!("{}", it.first) } else { format!("{}-{}", it.first, it.last) };
        let short = it.name.rsplit("::").next().unwrap_or(&it.name);
        format!("{}{range} {} {}\n", "  ".repeat(it.depth + 1), it.status, short)
    };
    let all: String = items.iter().map(row).collect();
    if all.len() <= max {
        out.push_str(&all);
    } else {
        // Over budget: each top-level item on its line, its members packed
        // as `name start` after it, then a count of what did not fit.
        let mut used = 0;
        let mut shown = 0;
        let mut k = 0;
        let leaf = |k: usize| items.get(k + 1).is_none_or(|n| n.depth <= items[k].depth);
        while k < items.len() {
            let it = &items[k];
            if matches!(it.status.as_str(), "const" | "static" | "type") && leaf(k) {
                // A run of top-level constants packs onto one line.
                let mut j = k;
                let mut names = Vec::new();
                while j < items.len() && items[j].status == it.status && items[j].depth == it.depth && leaf(j) {
                    names.push(format!("{} {}", items[j].name.rsplit("::").next().unwrap_or(&items[j].name), items[j].first));
                    j += 1;
                }
                let block = format!("{}{} {}\n", "  ".repeat(it.depth + 1), it.status, names.join(", "));
                if used + block.len() > max {
                    break;
                }
                used += block.len();
                shown += names.len();
                out.push_str(&block);
                k = j;
                continue;
            }
            let mut block = row(it);
            let mut members = Vec::new();
            let mut j = k + 1;
            while j < items.len() && items[j].depth > it.depth {
                let m = &items[j];
                members.push(format!("{} {}", m.name.rsplit("::").next().unwrap_or(&m.name), m.first));
                j += 1;
            }
            if !members.is_empty() {
                block.push_str(&format!("{}  {}\n", "  ".repeat(it.depth + 1), members.join(", ")));
            }
            if used + block.len() > max {
                break;
            }
            used += block.len();
            shown += 1 + members.len();
            out.push_str(&block);
            k = j;
        }
        out.push_str(&format!("({shown} of {} items shown; atlas slice {id} --item NAME)\n", items.len()));
    }
    out.push_str(&hint(id, "--item NAME | --lines A-B | --grep RE"));
    out
}

// ---- generic -------------------------------------------------------------

// ---- check logs ------------------------------------------------------------

/// The header of a per-package totals table: `package  passed  failed ...`.
fn table_head() -> Regex {
    re(r"^\s*(package|crate|suite)\s+passed\s+failed\b")
}

/// Verdict lines a summary keeps whole, up to this many characters.
const VERDICT: usize = 480;

/// The digest of a long check log, with no budget: only what decides it.
///
/// 1. Every totals table: from a [`table_head`] line to the first line
///    after it that starts with `total`, with the line before the header
///    when it is a short title.
/// 2. Failures: libtest `test NAME ... FAILED`, nextest `FAIL [..]` (and
///    SIGSEGV, SIGABRT, TIMEOUT), `failures:` lists' names, compiler
///    `error:` and `error[E..]:` lines with their ` --> ` location, and any
///    other line whose first word is `FAILED` or `FAIL`; each once, at most
///    [`MAX_FAILURES`] and then a count.
/// 3. Panics: each `panicked at` line and the message line after it, at
///    most [`MAX_FAILURES`] and then a count.
/// 4. Verdicts: lines of the form `NAME: PASS` or `NAME: FAIL` (a check's
///    own verdict, such as an oracle's), whole up to [`VERDICT`] characters.
pub fn summary(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let head = table_head();
    let mut out = String::new();
    let mut i = 0;
    let mut tables = 0;
    while i < lines.len() {
        if head.is_match(lines[i]) {
            let end = (i + 1..lines.len()).find(|&j| lines[j].trim_start().starts_with("total")).unwrap_or(lines.len() - 1);
            if i > 0 && !lines[i - 1].trim().is_empty() && lines[i - 1].len() <= 80 && !head.is_match(lines[i - 1]) {
                out.push_str(lines[i - 1].trim_end());
                out.push('\n');
            }
            for l in &lines[i..=end] {
                out.push_str(l.trim_end());
                out.push('\n');
            }
            tables += 1;
            i = end + 1;
        } else {
            i += 1;
        }
    }
    if tables == 0 {
        out.push_str("no totals table\n");
    }
    let fail = re(r"^test (\S+) \.\.\. FAILED|^\s*(FAIL|SIGSEGV|SIGABRT|TIMEOUT) \[|^error(\[E\d+\])?: |^\s*(FAILED|FAIL)\b");
    let mut failures: Vec<String> = Vec::new();
    let mut in_list = false;
    for (n, l) in lines.iter().enumerate() {
        let t = l.trim_end();
        if t == "failures:" {
            in_list = true;
            continue;
        }
        if in_list {
            if t.starts_with("    ") && !t.trim().is_empty() && !t.trim().contains(char::is_whitespace) {
                failures.push(t.trim().to_string());
                continue;
            }
            if !t.starts_with("    ") {
                in_list = false;
            }
        }
        if fail.is_match(t) && !t.starts_with("error: could not compile") && !t.starts_with("error: test failed") {
            let mut s = clip(t.trim(), QUOTE);
            if t.starts_with("error") {
                if let Some(loc) = lines.get(n + 1).filter(|x| x.trim_start().starts_with("--> ")) {
                    s = format!("{s} {}", loc.trim());
                }
            }
            failures.push(s);
        }
    }
    let mut seen = std::collections::HashSet::new();
    failures.retain(|f| seen.insert(f.clone()));
    if failures.is_empty() {
        out.push_str("failures: none\n");
    } else {
        out.push_str(&format!("failures ({}):\n", failures.len()));
        for f in failures.iter().take(MAX_FAILURES) {
            out.push_str(&format!("  {f}\n"));
        }
        if failures.len() > MAX_FAILURES {
            out.push_str(&format!("  and {} more\n", failures.len() - MAX_FAILURES));
        }
    }
    let mut panics: Vec<String> = Vec::new();
    for (n, l) in lines.iter().enumerate() {
        if l.contains("panicked at") {
            let msg = lines.get(n + 1).map(|m| m.trim()).filter(|m| !m.is_empty() && !m.starts_with("note:")).unwrap_or("");
            let p = if msg.is_empty() { clip(l.trim(), QUOTE) } else { format!("{} | {}", clip(l.trim(), QUOTE), clip(msg, QUOTE)) };
            if !panics.contains(&p) {
                panics.push(p);
            }
        }
    }
    if panics.is_empty() {
        out.push_str("panics: none\n");
    } else {
        out.push_str(&format!("panics ({}):\n", panics.len()));
        for p in panics.iter().take(MAX_FAILURES) {
            out.push_str(&format!("  {p}\n"));
        }
        if panics.len() > MAX_FAILURES {
            out.push_str(&format!("  and {} more\n", panics.len() - MAX_FAILURES));
        }
    }
    let verdict = re(r"^[^:]{1,80}: (PASS|FAIL)\b");
    for l in lines.iter().filter(|l| verdict.is_match(l.trim_start())) {
        out.push_str(&clip(l.trim(), VERDICT));
        out.push('\n');
    }
    out
}

/// `atlas digest [--as SHAPE] FILE`: the digest of a saved output (`-` reads
/// stdin), its shape detected from the text unless `--as` names one (test,
/// build, grep, listing, source, summary, generic). A check log with a
/// per-package totals table gets [`summary`].
///
/// # Errors
///
/// A usage error or an unreadable file.
pub fn cli(args: Vec<String>) -> Result<std::process::ExitCode, String> {
    const USAGE: &str = "usage: atlas digest [--as test|build|grep|listing|source|summary|generic] FILE|-";
    let mut it = args.into_iter();
    let mut forced: Option<Shape> = None;
    let mut file: Option<String> = None;
    while let Some(w) = it.next() {
        match w.as_str() {
            "--as" => forced = Some(crate::run::forced(&it.next().ok_or(USAGE)?)?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(std::process::ExitCode::SUCCESS);
            }
            _ if file.is_none() => file = Some(w),
            _ => return Err(USAGE.into()),
        }
    }
    let file = file.ok_or(USAGE)?;
    let bytes = if file == "-" {
        let mut b = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut b).map_err(|e| format!("stdin: {e}"))?;
        b
    } else {
        std::fs::read(&file).map_err(|e| format!("{file}: {e}"))?
    };
    let text = String::from_utf8_lossy(&bytes);
    let shape = forced.unwrap_or_else(|| shape("run", "", &text));
    let out = match shape {
        Shape::Summary => summary(&text),
        s => {
            let d = digest("file", &file, s, &text, None, DEFAULT_MAX_BYTES);
            // No handle backs a file: the slice hint does not apply.
            match d.trim_end().rsplit_once('\n') {
                Some((head, last)) if last.starts_with("atlas slice ") => format!("{head}\n"),
                _ => d,
            }
        }
    };
    print!("{out}");
    Ok(std::process::ExitCode::SUCCESS)
}

fn generic(id: &str, text: &str, exit: Option<i32>, max: usize) -> String {
    let bad = re(r"(?i)\b(error|fail(ed|ure)?|panic(ked)?)\b");
    let hits: Vec<(usize, &str)> = text.lines().enumerate().filter(|(_, l)| bad.is_match(l)).collect();
    let mut out = String::new();
    if exit.is_none() {
        out.push_str(&format!("{} lines match error|fail|panic; exit status unknown\n", hits.len()));
    } else {
        out.push_str(&format!("{} lines match error|fail|panic\n", hits.len()));
    }
    let budget = max / 2;
    let mut used = 0;
    for (i, l) in hits.iter().take(MAX_FAILURES) {
        let q = format!("{}: {}\n", i + 1, clip(l, QUOTE));
        if used + q.len() > budget {
            break;
        }
        used += q.len();
        out.push_str(&q);
    }
    out.push_str(&tail(text, max.saturating_sub(out.len()).max(200)));
    out.push_str(&hint(id, "--lines A-B | --grep RE"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_log_keeps_only_tables_failures_panics_and_verdicts() {
        let log = "xtask: cargo build --workspace
   Compiling a v0.1.0
noise line
test widgets::ok ... ok
        FAIL [   0.2s] pkg::tests widgets::bad
thread 'widgets::bad' panicked at src/w.rs:3:5:
assertion failed: x
note: run with RUST_BACKTRACE=1

fast tier
package   passed  failed  ignored     s
pkg            9       1        0   0.4
total          9       1        0   0.4
build 1.8 s, tests 2.0 s
xtask: render oracle `a`: field.exe --check (about a minute)
render oracle: PASS (`a` 6 pairs)
";
        assert_eq!(shape("run", "", log), Shape::Summary);
        let d = summary(log);
        assert_eq!(
            d,
            "fast tier
package   passed  failed  ignored     s
pkg            9       1        0   0.4
total          9       1        0   0.4
failures (1):
  FAIL [   0.2s] pkg::tests widgets::bad
panics (1):
  thread 'widgets::bad' panicked at src/w.rs:3:5: | assertion failed: x
render oracle: PASS (`a` 6 pairs)
"
        );
        assert!(summary("nothing here
").starts_with("no totals table
failures: none
panics: none
"));
    }

    #[test]
    fn shapes_follow_the_command_and_the_text() {
        assert_eq!(shape("bash", "cd x && RUST_LOG=1 cargo test -j 8", ""), Shape::CargoTest);
        assert_eq!(shape("bash", "cargo +nightly clippy", ""), Shape::CargoBuild);
        assert_eq!(shape("bash", "rg foo", "a.rs:1:foo\nb.rs:2:foo\n"), Shape::Grep);
        assert_eq!(shape("bash", "sed -n 1,9p a.rs", "fn a() {}\n"), Shape::Source(Lang::Brace));
        assert_eq!(shape("bash", "find .", "./a\n./b/c.rs\n"), Shape::Listing);
        assert_eq!(shape("bash", "make", "building things now\n"), Shape::Generic);
        assert_eq!(shape("read", "", "x"), Shape::Source(Lang::Auto));
        assert_eq!(shape("read", "/w/notes/a.md", "x"), Shape::Source(Lang::Markdown));
    }

    #[test]
    fn outlines_nest_items_with_end_lines() {
        let src = "use x;\n\npub struct A {\n    b: u8,\n}\n\nimpl A {\n    pub fn new() -> A {\n        A { b: 0 }\n    }\n\n    fn s(&self) -> &str { \"}\" }\n}\n";
        let o = outline(src);
        let names: Vec<(&str, usize, usize)> = o.iter().map(|i| (i.name.as_str(), i.first, i.last)).collect();
        assert_eq!(names, vec![("A", 3, 5), ("A", 7, 13), ("A::new", 8, 10), ("A::s", 12, 12)]);
    }

    #[test]
    fn item_ends_cover_signatures_fields_and_strings() {
        let src = [
            "pub struct Pair<A, B> {",  // 1
            "    pub a: A,",              // 2
            "    pub b: Vec<(A, B)>,",    // 3
            "}",                          // 4
            "pub fn long(",               // 5
            "    x: u32,",                // 6
            "    y: u32,",                // 7
            ") -> u32 {",                 // 8
            "    let s = \"}\";",         // 9
            "    x + y",                  // 10
            "}",                          // 11
            "const SQL: &str = \"",       // 12
            "SELECT 1; SELECT 2;",        // 13
            "\";",                        // 14
            "enum E {",                   // 15
            "    A,",                     // 16
            "    B",                      // 17
            "}",                          // 18
        ];
        let end = |l: usize| item_end(&src, l - 1);
        assert_eq!(end(1), 4);
        assert_eq!(end(2), 2);
        assert_eq!(end(3), 3);
        assert_eq!(end(5), 11);
        assert_eq!(end(12), 14);
        assert_eq!(end(16), 16);
        assert_eq!(end(17), 17, "the last variant ends before its container closes");
    }

    #[test]
    fn markdown_sections_end_before_the_next_peer() {
        let md = "# T\n\nx\n## a\n1\n## b\n2\n# U\n";
        let o = outline(md);
        let v: Vec<(&str, usize, usize)> = o.iter().map(|i| (i.name.as_str(), i.first, i.last)).collect();
        assert_eq!(v, vec![("T", 1, 7), ("a", 4, 5), ("b", 6, 7), ("U", 8, 8)]);
    }

    #[test]
    fn grep_paths_keep_drive_letters() {
        assert_eq!(grep_path("C:/w/a.rs:12:x"), Some("C:/w/a.rs"));
        assert_eq!(grep_path("src/a.rs-3-ctx"), Some("src/a.rs"));
        assert_eq!(grep_path("no hit here"), None);
    }

    #[test]
    fn digests_are_reproducible() {
        let t = "a\nb\nerror: x\n";
        assert_eq!(digest("h:1", "make", Shape::Generic, t, None, 600), digest("h:1", "make", Shape::Generic, t, None, 600));
    }
}
