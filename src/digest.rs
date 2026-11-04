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

use crate::lex;
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
    /// The output of another test runner of [`RUNNERS`].
    Test(&'static Runner),
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

/// A pattern of a [`RUNNERS`] row. The digest's own fixed patterns are the
/// hand-written scanners below.
fn re(p: &str) -> Regex {
    Regex::new(p).expect("digest pattern")
}

// ---- line scanners ---------------------------------------------------------
//
// Each names the regex it replaced; `lex/props.rs` checks they agree.

/// Group spans of a scanner.
type Spans<const N: usize> = [lex::Span; N];

#[cfg(test)]
pub(crate) fn lex_sites() -> Vec<lex::props::Site> {
    use lex::props::{v, Mode, Site};
    fn site(name: &'static str, pattern: &str, tokens: &'static [&'static str], examples: &'static [&'static str], mode: Mode) -> Site {
        Site { name, pattern: pattern.into(), tokens, examples, mode }
    }
    const CMD: &[&str] = &[
        "cargo", " ", "+nightly", "+", "test", "nextest", "t", "build", "check", "clippy", "b", "c", "run", "r", "doc", "x", "_", "-p", "s",
    ];
    const LOG: &[&str] = &[
        "test ",
        " ... ",
        "ok",
        "FAILED",
        "ignored",
        "---- ",
        " ----",
        "stdout",
        "stderr",
        "--- ",
        "STDOUT",
        "STDERR",
        ": ",
        " ---",
        "PASS",
        "FAIL",
        "SIGSEGV",
        "TIMEOUT",
        "[",
        "]",
        "(",
        ")",
        "/",
        "1",
        "12",
        "0.5s",
        " ",
        "  ",
        "a::b",
        "x",
        "error",
        "warning",
        "[E0308]",
        "-->",
        "src/a.rs:1:2",
        "panicked at ",
        "'",
        "', ",
        ":",
        "Summary ",
        "tests",
        " run: ",
        " passed",
        ", ",
        " failed",
        " skipped",
        "test result: ",
        "passed; ",
        "failed; ",
    ];
    vec![
        site(
            "digest cargo test command",
            r"\bcargo\s+(\+\S+\s+)?(test|nextest|t)\b",
            CMD,
            &["cargo test -p x", "x && cargo +nightly nextest run", "cargo t"],
            Mode::Bool(cargo_test_cmd),
        ),
        site(
            "digest cargo build command",
            r"\bcargo\s+(\+\S+\s+)?(build|check|clippy|b|c|run|r|doc)\b",
            CMD,
            &["cargo build --release", "cargo +stable clippy", "xcargo b"],
            Mode::Bool(cargo_build_cmd),
        ),
        site(
            "digest grep hit",
            r"^[^\s:][^:]*:\d+[:-]",
            &["src/a.rs", ":", "12", "-", " ", "a", "\u{663}"],
            &["src/a.rs:12:fn x", "a b-3-ctx", "C:/x:1:"],
            Mode::Bool(grep_hit),
        ),
        site(
            "digest reader command",
            r"^(cat|head|tail|sed|nl|less|more|type|Get-Content|gc)\b",
            &["cat", "head", "Get-Content", "gc", " ", "x", "-", "_"],
            &["cat a.rs", "Get-Content x", "sed -n 1p"],
            Mode::Bool(reader_cmd),
        ),
        site(
            "digest path line",
            r"^\.?[\w@.+~/\\:-]+$",
            &[".", "/", "a", "\\", ":", "@", "+", "~", "-", " ", "\u{e9}"],
            &["./src/a.rs", "C:\\x\\y.txt", "."],
            Mode::Bool(path_line),
        ),
        site(
            "digest test result",
            r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored",
            LOG,
            &["test result: ok. 12 passed; 0 failed; 1 ignored; 0 measured"],
            Mode::One(|s| test_result(s).map(v)),
        ),
        site(
            "digest nextest summary",
            r"^\s*Summary \[.*\] (\d+) tests? run: (\d+) passed(?:, (\d+) failed)?(?:.*?(\d+) skipped)?",
            LOG,
            &[
                "     Summary [   1.234s] 12 tests run: 10 passed, 2 failed, 3 skipped",
                "Summary [ 0.1s] 1 test run: 1 passed (1 slow), 4 skipped",
            ],
            Mode::One(|s| nextest_summary(s).map(v)),
        ),
        site(
            "digest libtest status",
            r"^test (\S+) \.\.\. (ok|FAILED|ignored)",
            LOG,
            &["test a::b ... ok", "test x ... FAILED"],
            Mode::One(|s| libtest_status(s).map(v)),
        ),
        site(
            "digest libtest block",
            r"^---- (\S+) (stdout|stderr) ----$",
            LOG,
            &["---- a::b stdout ----"],
            Mode::One(|s| libtest_block(s).map(v)),
        ),
        site(
            "digest nextest status",
            r"^\s+(PASS|FAIL|SIGSEGV|SIGABRT|TIMEOUT) \[[^\]]*\] (?:\(\s*\d+/\d+\)\s+)?(\S+) (\S+)\s*$",
            LOG,
            &["        PASS [   0.012s] atlas a::b", "        FAIL [   0.5s] ( 1/12) atlas x  ", "  PASS [ 1s] (1/2) a"],
            Mode::One(|s| nextest_status(s).map(v)),
        ),
        site(
            "digest nextest block",
            r"^--- (STDOUT|STDERR): +(\S+) (\S+) ---$",
            LOG,
            &["--- STDOUT:              atlas a::b ---"],
            Mode::One(|s| nextest_block(s).map(v)),
        ),
        site(
            "digest panic (new)",
            r"panicked at ([^\s]+:\d+):\d+:$",
            LOG,
            &["thread 'a' panicked at src/a.rs:12:5:", "panicked at C:\\x.rs:1:2:", "panicked at a:b:1:2:"],
            Mode::One(|s| panic_new(s).map(v)),
        ),
        site(
            "digest panic (old)",
            r"panicked at '(.*)', ([^\s]+:\d+):\d+",
            LOG,
            &["thread 'a' panicked at 'boom', src/a.rs:12:5", "panicked at 'x', y', src/b.rs:1:2 more"],
            Mode::One(|s| panic_old(s).map(v)),
        ),
        site(
            "digest build head",
            r"^(error|warning)(\[[A-Za-z0-9_:]+\])?: (.*)$",
            LOG,
            &["error[E0308]: mismatched types", "warning: unused variable: `x`"],
            Mode::One(|s| build_head(s).map(v)),
        ),
        site(
            "digest build skip",
            r"^(error: could not compile|error: aborting|warning: build failed|warning: `[^`]+` \(.*\) generated|warning: unused manifest key|error: test failed|error: \d+ target)",
            &[
                "error: ",
                "warning: ",
                "`",
                "` (",
                ") generated",
                "could not compile",
                "aborting",
                "build failed",
                "unused manifest key",
                "test failed",
                "12",
                " target",
                "x",
                "\n",
            ],
            &["warning: `atlas` (lib) generated 2 warnings", "error: could not compile `x`", "error: 2 targets failed"],
            Mode::Bool(build_skip),
        ),
        site(
            "digest build location",
            r"^\s*--> ([^\s]+):(\d+):\d+",
            LOG,
            &["  --> src/a.rs:12:5", "--> C:\\x.rs:1:2:3"],
            Mode::One(|s| build_at(s).map(v)),
        ),
        site(
            "digest markdown heading",
            r"^(#{1,6}) +(\S.*?)\s*#*\s*$",
            &["#", "##", " ", "  ", "a", "\t", "#######", "\n"],
            &["## Title ##", "# A # b  #  ", "###### x"],
            Mode::One(|s| md_heading(s).map(v)),
        ),
        site(
            "digest python definition",
            r"^(\s*)(async\s+def|def|class)\s+(\w+)",
            &["async", "def", "class", " ", "\t", "x", "_", "\u{e9}", "("],
            &["    async def run(self):", "class A(B):", "def f():"],
            Mode::One(|s| py_def(s).map(v)),
        ),
        site(
            "digest code declaration",
            r"^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|default|extern\s+\S+)\s+)*(fn|struct|enum|trait|impl|mod|type|const|static|union|macro_rules!|class|interface|function|namespace)\b\s*(.*)$",
            &[
                "pub",
                "(crate)",
                " ",
                "async",
                "const",
                "unsafe",
                "default",
                "extern",
                "\"C\"",
                "fn",
                "struct",
                "impl",
                "mod",
                "macro_rules!",
                "class",
                "function",
                "namespace",
                "x",
                "{",
                "\n",
                "<T>",
            ],
            &[
                "pub(crate) async fn x() {",
                "  impl<T> A for B<T> {",
                "const X: u8 = 1;",
                "extern \"C\" fn f()",
                "macro_rules!m",
                "export function f() {",
            ],
            Mode::One(|s| code_decl(s).map(v)),
        ),
        site(
            "digest outline name",
            r"^(?:<[^>]*>\s*)?([A-Za-z_][A-Za-z0-9_]*)",
            &["<", ">", "<T>", " ", "x", "_", "1", "\u{e9}"],
            &["<T> Foo<T>", "bar_1(", "<'a>  Baz"],
            Mode::One(|s| outline_ident(s).map(v)),
        ),
        site(
            "digest table head",
            r"^\s*(package|crate|suite)\s+passed\s+failed\b",
            &["package", "crate", "suite", " ", "passed", "failed", "x", "_"],
            &["  package   passed  failed  ignored", "crate passed failed"],
            Mode::Bool(table_head),
        ),
        site(
            "digest summary failure",
            r"^test (\S+) \.\.\. FAILED|^\s*(FAIL|SIGSEGV|SIGABRT|TIMEOUT) \[|^error(\[E\d+\])?: |^\s*(FAILED|FAIL)\b",
            &["test ", " ... FAILED", "FAIL", "FAILED", "ED", " [", "error", "[E", "0308", "]", ": ", " ", "x", "SIGSEGV", "TIMEOUT"],
            &["test a::b ... FAILED", "   FAIL [ 1s] x", "error[E0308]: y", "FAILED: z", "error: x"],
            Mode::Bool(summary_fail),
        ),
        site(
            "digest verdict line",
            r"^[^:]{1,80}: (PASS|FAIL)\b",
            &["oracle", ": ", "PASS", "FAIL", ":", " ", "x", "PASSED"],
            &["shadow oracle: PASS", "a b: FAIL (3)"],
            Mode::Bool(verdict_line),
        ),
        site(
            "digest error word",
            r"(?i)\b(error|fail(ed|ure)?|panic(ked)?)\b",
            &["error", "ERROR", "Fail", "failed", "FAILURE", "panic", "Panicked", "x", "_", " ", "\u{212a}", "\u{e9}", "1"],
            &["an Error here", "it FAILED.", "panicked!"],
            Mode::Bool(bad_word),
        ),
    ]
}

/// `\bcargo\s+(\+\S+\s+)?(SUB)\b` for any of `subs`, anywhere.
fn cargo_sub(s: &str, subs: &[&str]) -> bool {
    let mut q = 0;
    std::iter::from_fn(|| {
        let p = lex::find_str(s, q, "cargo")?;
        q = p + 5;
        Some(p)
    })
    .any(|p| {
        if lex::before(s, p).is_some_and(lex::word) {
            return false;
        }
        let Some(mut n) = lex::spaces1(s, p + 5) else { return false };
        if s.as_bytes().get(n) == Some(&b'+') {
            match lex::run1(s, n + 1, |c| !lex::space(c)).and_then(|m| lex::spaces1(s, m)) {
                Some(m) => n = m,
                None => return false,
            }
        }
        subs.iter().any(|w| lex::lit(s, n, w).is_some_and(|e| lex::boundary(s, e)))
    })
}

/// `\bcargo\s+(\+\S+\s+)?(test|nextest|t)\b`.
pub(crate) fn cargo_test_cmd(s: &str) -> bool {
    cargo_sub(s, &["test", "nextest", "t"])
}

/// `\bcargo\s+(\+\S+\s+)?(build|check|clippy|b|c|run|r|doc)\b`.
pub(crate) fn cargo_build_cmd(s: &str) -> bool {
    cargo_sub(s, &["build", "check", "clippy", "b", "c", "run", "r", "doc"])
}

/// `^[^\s:][^:]*:\d+[:-]`: a grep hit or context line.
pub(crate) fn grep_hit(s: &str) -> bool {
    let Some(c) = lex::at(s, 0).filter(|&c| !lex::space(c) && c != ':') else { return false };
    let Some(k) = lex::find_byte(s, c.len_utf8(), b':') else { return false };
    lex::run1(s, k + 1, lex::digit).is_some_and(|d| matches!(s.as_bytes().get(d), Some(b':' | b'-')))
}

/// `^(cat|head|tail|sed|nl|less|more|type|Get-Content|gc)\b`.
pub(crate) fn reader_cmd(s: &str) -> bool {
    ["cat", "head", "tail", "sed", "nl", "less", "more", "type", "Get-Content", "gc"]
        .iter()
        .any(|w| lex::lit(s, 0, w).is_some_and(|e| lex::boundary(s, e)))
}

/// `^\.?[\w@.+~/\\:-]+$`: a path alone on its line.
pub(crate) fn path_line(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| lex::word(c) || matches!(c, '@' | '.' | '+' | '~' | '/' | '\\' | ':' | '-'))
}

/// `^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored`.
pub(crate) fn test_result(s: &str) -> Option<Spans<4>> {
    let w = lex::run1(s, lex::lit(s, 0, "test result: ")?, lex::word)?;
    let a0 = lex::lit(s, w, ". ")?;
    let a1 = lex::run1(s, a0, lex::digit)?;
    let b0 = lex::lit(s, a1, " passed; ")?;
    let b1 = lex::run1(s, b0, lex::digit)?;
    let c0 = lex::lit(s, b1, " failed; ")?;
    let c1 = lex::run1(s, c0, lex::digit)?;
    let e = lex::lit(s, c1, " ignored")?;
    Some([Some((0, e)), Some((a0, a1)), Some((b0, b1)), Some((c0, c1))])
}

/// `^\s*Summary \[.*\] (\d+) tests? run: (\d+) passed(?:, (\d+) failed)?(?:.*?(\d+) skipped)?`:
/// nextest's summary line.
pub(crate) fn nextest_summary(s: &str) -> Option<Spans<5>> {
    let i = lex::lit(s, lex::spaces(s, 0), "Summary [")?;
    let le = lex::line_end(s, i);
    // The bracket that ends `.*`: the last one the rest follows.
    let (run, passed, mut e) = s[i..le].rmatch_indices(']').find_map(|(k, _)| {
        let r0 = lex::lit(s, i + k, "] ")?;
        let r1 = lex::run1(s, r0, lex::digit)?;
        let t = lex::lit(s, r1, " test")?;
        let t = lex::lit(s, t, "s").unwrap_or(t);
        let p0 = lex::lit(s, t, " run: ")?;
        let p1 = lex::run1(s, p0, lex::digit)?;
        Some(((r0, r1), (p0, p1), lex::lit(s, p1, " passed")?))
    })?;
    let mut failed = None;
    if let Some(f0) = lex::lit(s, e, ", ") {
        if let Some(f1) = lex::run1(s, f0, lex::digit) {
            if let Some(f) = lex::lit(s, f1, " failed") {
                failed = Some((f0, f1));
                e = f;
            }
        }
    }
    // `.*?(\d+) skipped`: the first digits that `skipped` follows.
    let le = lex::line_end(s, e);
    let mut skipped = None;
    for (t, _) in s[e..le].match_indices(" skipped") {
        let t = e + t;
        let r = s[e..t].char_indices().rev().take_while(|&(_, c)| lex::digit(c)).last().map_or(t, |(k, _)| e + k);
        if r < t {
            skipped = Some((r, t));
            e = t + 8;
            break;
        }
    }
    Some([Some((0, e)), Some(run), Some(passed), failed, skipped])
}

/// `^test (\S+) \.\.\. (ok|FAILED|ignored)`: a libtest result line.
pub(crate) fn libtest_status(s: &str) -> Option<Spans<3>> {
    let n0 = lex::lit(s, 0, "test ")?;
    let n1 = lex::run1(s, n0, |c| !lex::space(c))?;
    let w0 = lex::lit(s, n1, " ... ")?;
    let (w1, _) = lex::one_of(s, w0, &["ok", "FAILED", "ignored"])?;
    Some([Some((0, w1)), Some((n0, n1)), Some((w0, w1))])
}

/// `^---- (\S+) (stdout|stderr) ----$`: a libtest output block's header.
pub(crate) fn libtest_block(s: &str) -> Option<Spans<3>> {
    let n0 = lex::lit(s, 0, "---- ")?;
    let n1 = lex::run1(s, n0, |c| !lex::space(c))?;
    let w0 = lex::lit(s, n1, " ")?;
    let (w1, _) = lex::one_of(s, w0, &["stdout", "stderr"])?;
    (lex::lit(s, w1, " ----")? == s.len()).then_some([Some((0, s.len())), Some((n0, n1)), Some((w0, w1))])
}

/// `^\s+(PASS|FAIL|SIGSEGV|SIGABRT|TIMEOUT) \[[^\]]*\] (?:\(\s*\d+/\d+\)\s+)?(\S+) (\S+)\s*$`:
/// a nextest result line.
pub(crate) fn nextest_status(s: &str) -> Option<Spans<4>> {
    let w0 = lex::spaces1(s, 0)?;
    let (w1, _) = lex::one_of(s, w0, &["PASS", "FAIL", "SIGSEGV", "SIGABRT", "TIMEOUT"])?;
    let b = lex::lit(s, w1, " [")?;
    let p = lex::lit(s, lex::find_byte(s, b, b']')?, "] ")?;
    // The `(1/2)` counter is optional: with it first, then without.
    let counter = lex::lit(s, p, "(")
        .map(|q| lex::spaces(s, q))
        .and_then(|q| lex::run1(s, q, lex::digit))
        .and_then(|q| lex::lit(s, q, "/"))
        .and_then(|q| lex::run1(s, q, lex::digit))
        .and_then(|q| lex::lit(s, q, ")"))
        .and_then(|q| lex::spaces1(s, q));
    [counter, Some(p)].into_iter().flatten().find_map(|a0| {
        let a1 = lex::run1(s, a0, |c| !lex::space(c))?;
        let b0 = lex::lit(s, a1, " ")?;
        let b1 = lex::run1(s, b0, |c| !lex::space(c))?;
        (lex::spaces(s, b1) == s.len()).then_some([Some((0, s.len())), Some((w0, w1)), Some((a0, a1)), Some((b0, b1))])
    })
}

/// `^--- (STDOUT|STDERR): +(\S+) (\S+) ---$`: a nextest output block's header.
pub(crate) fn nextest_block(s: &str) -> Option<Spans<4>> {
    let w0 = lex::lit(s, 0, "--- ")?;
    let (w1, _) = lex::one_of(s, w0, &["STDOUT", "STDERR"])?;
    let a0 = lex::run1(s, lex::lit(s, w1, ":")?, |c| c == ' ')?;
    let a1 = lex::run1(s, a0, |c| !lex::space(c))?;
    let b0 = lex::lit(s, a1, " ")?;
    let b1 = lex::run1(s, b0, |c| !lex::space(c))?;
    (lex::lit(s, b1, " ---")? == s.len()).then_some([Some((0, s.len())), Some((w0, w1)), Some((a0, a1)), Some((b0, b1))])
}

/// `panicked at ([^\s]+:\d+):\d+:$`: a panic line (Rust 1.73 on).
pub(crate) fn panic_new(s: &str) -> Option<Spans<2>> {
    // Every earlier `panicked at ` has this one's spaces after it.
    let p = s.rfind("panicked at ")?;
    let t0 = p + 12;
    let t = s[t0..].strip_suffix(':')?;
    if t.contains(lex::space) {
        return None;
    }
    let (rest, col) = t.rsplit_once(':')?;
    let (path, line) = rest.rsplit_once(':')?;
    let digits = |x: &str| !x.is_empty() && x.chars().all(lex::digit);
    (!path.is_empty() && digits(line) && digits(col)).then_some([Some((p, s.len())), Some((t0, t0 + rest.len()))])
}

/// `panicked at '(.*)', ([^\s]+:\d+):\d+`: a panic line (before Rust 1.73).
pub(crate) fn panic_old(s: &str) -> Option<Spans<3>> {
    let mut q = 0;
    while let Some(p) = lex::find_str(s, q, "panicked at '") {
        let m0 = p + 13;
        let le = lex::line_end(s, m0);
        // `.*` is greedy: the last `', ` that a location follows.
        let hit = s[m0..le].rmatch_indices("', ").find_map(|(k, _)| {
            let k = m0 + k;
            lex::path_line_col(s, k + 3).map(|(_, d)| (k, d))
        });
        if let Some((k, d)) = hit {
            let end = lex::run(s, d + 1, lex::digit);
            return Some([Some((p, end)), Some((m0, k)), Some((k + 3, d))]);
        }
        // A later opener on this line has less of it to search.
        q = le.max(p + 1);
    }
    None
}

/// `^(error|warning)(\[[A-Za-z0-9_:]+\])?: (.*)$`: a compiler message's
/// first line.
pub(crate) fn build_head(s: &str) -> Option<Spans<4>> {
    let (w, _) = lex::one_of(s, 0, &["error", "warning"])?;
    let code = lex::lit(s, w, "[")
        .and_then(|q| lex::run1(s, q, |c| c.is_ascii_alphanumeric() || c == '_' || c == ':'))
        .and_then(|q| lex::lit(s, q, "]"))
        .map(|q| (w, q));
    let m0 = lex::lit(s, code.map_or(w, |c| c.1), ": ")?;
    (!s[m0..].contains('\n')).then_some([Some((0, s.len())), Some((0, w)), code, Some((m0, s.len()))])
}

/// `^(error: could not compile|error: aborting|warning: build failed|warning: `[^`]+` \(.*\) generated|warning: unused manifest key|error: test failed|error: \d+ target)`:
/// a summary line, not a message.
pub(crate) fn build_skip(s: &str) -> bool {
    let generated = lex::lit(s, 0, "warning: `")
        .and_then(|q| lex::run1(s, q, |c| c != '`'))
        .and_then(|q| lex::lit(s, q, "` ("))
        .is_some_and(|q| lex::find_str(&s[..lex::line_end(s, q)], q, ") generated").is_some());
    let targets = lex::lit(s, 0, "error: ").and_then(|q| lex::run1(s, q, lex::digit)).is_some_and(|q| lex::lit(s, q, " target").is_some());
    generated
        || targets
        || ["error: could not compile", "error: aborting", "warning: build failed", "warning: unused manifest key", "error: test failed"]
            .iter()
            .any(|p| s.starts_with(p))
}

/// `^\s*--> ([^\s]+):(\d+):\d+`: a message's location line.
pub(crate) fn build_at(s: &str) -> Option<Spans<3>> {
    let x0 = lex::lit(s, lex::spaces(s, 0), "--> ")?;
    let (x, d) = lex::path_line_col(s, x0)?;
    Some([Some((0, lex::run(s, d + 1, lex::digit))), Some((x0, x)), Some((x + 1, d))])
}

/// `^(#{1,6}) +(\S.*?)\s*#*\s*$`: a Markdown heading, its text without
/// closing hashes.
pub(crate) fn md_heading(s: &str) -> Option<Spans<3>> {
    let (h, n) = lex::run_max(s, 0, 7, |c| c == '#');
    if !(1..=6).contains(&n) {
        return None;
    }
    let t0 = lex::run1(s, h, |c| c == ' ')?;
    let c0 = lex::at(s, t0).filter(|&c| !lex::space(c))?;
    // The shortest text after which only spaces, hashes, spaces remain.
    let a = s.trim_end_matches(lex::space).len();
    let b = s[..a].trim_end_matches('#').len();
    let c = s[..b].trim_end_matches(lex::space).len();
    let t1 = c.max(t0 + c0.len_utf8());
    (!s[t0..t1].contains('\n')).then_some([Some((0, s.len())), Some((0, h)), Some((t0, t1))])
}

/// `^(\s*)(async\s+def|def|class)\s+(\w+)`: a Python definition.
pub(crate) fn py_def(s: &str) -> Option<Spans<4>> {
    let i = lex::spaces(s, 0);
    let k = lex::lit(s, i, "async")
        .and_then(|q| lex::spaces1(s, q))
        .and_then(|q| lex::lit(s, q, "def"))
        .or_else(|| lex::one_of(s, i, &["def", "class"]).map(|w| w.0))?;
    let n0 = lex::spaces1(s, k)?;
    let n1 = lex::run1(s, n0, lex::word)?;
    Some([Some((0, n1)), Some((0, i)), Some((i, k)), Some((n0, n1))])
}

/// `^(\s*)(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|default|extern\s+\S+)\s+)*(fn|struct|enum|trait|impl|mod|type|const|static|union|macro_rules!|class|interface|function|namespace)\b\s*(.*)$`:
/// a declaration in a brace language.
pub(crate) fn code_decl(s: &str) -> Option<Spans<4>> {
    let ind = lex::spaces(s, 0);
    let mut i = ind;
    if let Some(p) = lex::lit(s, i, "pub") {
        let parens = (s.as_bytes().get(p) == Some(&b'(')).then(|| s[p + 1..].find(')').map(|k| p + 2 + k)).flatten();
        if let Some(a) = parens.and_then(|q| lex::spaces1(s, q)).or_else(|| lex::spaces1(s, p)) {
            i = a;
        }
    }
    let mut starts = vec![i];
    loop {
        let x = *starts.last().expect("one start");
        let Some((e, w)) = lex::one_of(s, x, &["async", "const", "unsafe", "default", "extern"]) else { break };
        let e = if w == "extern" { lex::spaces1(s, e).and_then(|q| lex::run1(s, q, |c| !lex::space(c))) } else { Some(e) };
        match e.and_then(|e| lex::spaces1(s, e)) {
            Some(n) => starts.push(n),
            None => break,
        }
    }
    // No keyword is a prefix of another, so at most one matches.
    const KW: &[&str] = &[
        "fn",
        "struct",
        "enum",
        "trait",
        "impl",
        "mod",
        "type",
        "const",
        "static",
        "union",
        "macro_rules!",
        "class",
        "interface",
        "function",
        "namespace",
    ];
    starts.iter().rev().find_map(|&x| {
        let (e, _) = lex::one_of(s, x, KW).filter(|&(e, _)| lex::boundary(s, e))?;
        let r = lex::spaces(s, e);
        (!s[r..].contains('\n')).then_some([Some((0, s.len())), Some((0, ind)), Some((x, e)), Some((r, s.len()))])
    })
}

/// `^(?:<[^>]*>\s*)?([A-Za-z_][A-Za-z0-9_]*)`: a name after optional
/// generics.
pub(crate) fn outline_ident(s: &str) -> Option<Spans<2>> {
    let n0 = if s.starts_with('<') { lex::spaces(s, s.find('>')? + 1) } else { 0 };
    let c = lex::at(s, n0).filter(|&c| lex::ident_start(c))?;
    let n1 = lex::run(s, n0 + c.len_utf8(), lex::ident);
    Some([Some((0, n1)), Some((n0, n1))])
}

/// `^\s*(package|crate|suite)\s+passed\s+failed\b`: a totals table's header.
pub(crate) fn table_head(s: &str) -> bool {
    lex::one_of(s, lex::spaces(s, 0), &["package", "crate", "suite"])
        .and_then(|(e, _)| lex::spaces1(s, e))
        .and_then(|q| lex::lit(s, q, "passed"))
        .and_then(|q| lex::spaces1(s, q))
        .and_then(|q| lex::lit(s, q, "failed"))
        .is_some_and(|e| lex::boundary(s, e))
}

/// `^test (\S+) \.\.\. FAILED|^\s*(FAIL|SIGSEGV|SIGABRT|TIMEOUT) \[|^error(\[E\d+\])?: |^\s*(FAILED|FAIL)\b`:
/// a line a check log's summary lists as a failure.
pub(crate) fn summary_fail(s: &str) -> bool {
    let libtest =
        lex::lit(s, 0, "test ").and_then(|q| lex::run1(s, q, |c| !lex::space(c))).is_some_and(|q| lex::lit(s, q, " ... FAILED").is_some());
    let i = lex::spaces(s, 0);
    let nextest = lex::one_of(s, i, &["FAIL", "SIGSEGV", "SIGABRT", "TIMEOUT"]).is_some_and(|(e, _)| lex::lit(s, e, " [").is_some());
    let error = lex::lit(s, 0, "error").is_some_and(|e| {
        let code = lex::lit(s, e, "[E").and_then(|q| lex::run1(s, q, lex::digit)).and_then(|q| lex::lit(s, q, "]"));
        lex::lit(s, code.unwrap_or(e), ": ").is_some()
    });
    let word = lex::lit(s, i, "FAIL").is_some_and(|e| lex::lit(s, e, "ED").is_some_and(|f| lex::boundary(s, f)) || lex::boundary(s, e));
    libtest || nextest || error || word
}

/// `^[^:]{1,80}: (PASS|FAIL)\b`: a check's own verdict line.
pub(crate) fn verdict_line(s: &str) -> bool {
    let (e, n) = lex::run_max(s, 0, 81, |c| c != ':');
    (1..=80).contains(&n)
        && lex::lit(s, e, ": ").and_then(|q| lex::one_of(s, q, &["PASS", "FAIL"])).is_some_and(|(f, _)| lex::boundary(s, f))
}

/// `(?i)\b(error|fail(ed|ure)?|panic(ked)?)\b`: a word that says
/// something went wrong, in any ASCII case.
pub(crate) fn bad_word(s: &str) -> bool {
    // `\b` both sides: the word is a whole `\w` run. Runs of 4 to 8 ASCII
    // letters are lowercased into a stack buffer and binary searched.
    const WORDS: &[&[u8]] = &[b"error", b"fail", b"failed", b"failure", b"panic", b"panicked"];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c < 0x80 && !lex::WORD.has(c) {
            i += 1;
            continue;
        }
        let e = lex::run(s, i, lex::word);
        if e == i {
            // A non-ASCII character that is not a word character.
            i += lex::at(s, i).map_or(1, char::len_utf8);
            continue;
        }
        if (4..=8).contains(&(e - i)) {
            let mut low = [0u8; 8];
            for (d, x) in low.iter_mut().zip(&b[i..e]) {
                *d = x.to_ascii_lowercase();
            }
            if WORDS.binary_search(&&low[..e - i]).is_ok() {
                return true;
            }
        }
        i = e;
    }
    false
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
    if text.lines().any(table_head) {
        return Shape::Summary;
    }
    if cargo_test_cmd(cmd) || text.contains("\ntest result: ") {
        return Shape::CargoTest;
    }
    if let Some(r) = runner_of(cmd, text) {
        return Shape::Test(r);
    }
    if cargo_build_cmd(cmd) {
        return Shape::CargoBuild;
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(200).collect();
    if !lines.is_empty() {
        let grep_like = lines.iter().filter(|l| grep_hit(l)).count();
        if grep_like * 10 >= lines.len() * 6 {
            return Shape::Grep;
        }
    }
    if reader_cmd(step) {
        return Shape::Source(lang_of(step));
    }
    if !lines.is_empty() {
        let paths = lines.iter().filter(|l| path_line(l.trim_end())).count();
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
        Shape::Test(r) => runner_test(r, id, text, exit, max_bytes),
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
        Shape::Test(r) => runner_items(r, text),
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
    let mut t = Totals::default();
    for l in text.lines() {
        let lt = l.trim_start();
        if let Some(c) = test_result(lt) {
            let g = |i: usize| c[i].and_then(|(a, b)| lt[a..b].parse::<u64>().ok()).unwrap_or(0);
            t.passed += g(1);
            t.failed += g(2);
            t.ignored += g(3);
            t.binaries += 1;
        } else if let Some(c) = nextest_summary(l) {
            let g = |i: usize| c[i].and_then(|(a, b)| l[a..b].parse::<u64>().ok()).unwrap_or(0);
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
    let mut v = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let g = |c: lex::Span| c.map_or("", |(a, b)| &l[a..b]);
        if let Some(c) = libtest_status(l) {
            let st = match g(c[2]) {
                "FAILED" => "failed",
                "ignored" => "ignored",
                _ => "ok",
            };
            v.push(Item { name: g(c[1]).to_string(), first: i + 1, last: i + 1, status: st.into(), depth: 0 });
        } else if let Some(c) = nextest_status(l) {
            let st = if g(c[1]) == "PASS" { "ok" } else { "failed" };
            v.push(Item { name: g(c[3]).to_string(), first: i + 1, last: i + 1, status: st.into(), depth: 0 });
        } else if let Some(name) = libtest_block(l).map(|c| g(c[1])).or_else(|| nextest_block(l).map(|c| g(c[3]))) {
            let name = name.to_string();
            let mut j = i + 1;
            while j < lines.len() {
                let m = lines[j];
                if libtest_block(m).is_some()
                    || nextest_block(m).is_some()
                    || m.starts_with("failures:")
                    || m.starts_with("test result:")
                    || nextest_status(m).is_some()
                {
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
    for (k, l) in lines.iter().enumerate() {
        let g = |c: lex::Span| c.map_or("", |(a, b)| &l[a..b]);
        if let Some(c) = panic_new(l) {
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
            return format!("{} {}", g(c[1]).replace('\\', "/"), clip(&msg, QUOTE));
        }
        if let Some(c) = panic_old(l) {
            return format!("{} {}", g(c[2]).replace('\\', "/"), clip(g(c[1]), QUOTE));
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

// ---- other test runners --------------------------------------------------

/// A test runner's row: how its output is recognised and read. The digest
/// and the items ([`runner_items`]) are one implementation for every row;
/// adding a runner is adding a row to [`RUNNERS`]. Every pattern is over
/// one line ([`crate::regex`] syntax), after terminal colour codes are
/// taken out (runners colour their output on Windows even into a pipe).
#[derive(Debug, PartialEq, Eq)]
pub struct Runner {
    /// The name the digest prints (`go test`).
    pub name: &'static str,
    /// A command that runs it.
    pub command: &'static str,
    /// A line that only its output has, for output without a command (and
    /// to choose between rows whose command both match: `go test -json`).
    pub detect: &'static str,
    /// For a runner that writes JSON lines (`go test -json`): the field of
    /// each line holding the text the patterns read; empty for plain text.
    /// Item lines stay those of the JSON lines.
    pub json_text: &'static str,
    /// For a JSON-lines runner, the field whose value groups the lines
    /// (`Package`: packages run at once and interleave their output); each
    /// group's text is read whole, in the order the groups first appear.
    pub json_group: &'static str,
    /// A test's result line: groups `status` and `name`.
    pub status: &'static str,
    /// The `status` words for a pass, a failure and a skip, each a
    /// `|`-separated list (`✓|√`).
    pub words: [&'static str; 3],
    /// A test's start line (its output follows until a `stop` line):
    /// group `name`; empty when the runner prints none.
    pub start: &'static str,
    /// The first line of a failed test's report (jest's `● Suite › test`,
    /// vitest's `FAIL  file > test`): the test failed, and its report
    /// follows until a `stop` line; group `name`. Empty when the runner
    /// reports failures on result lines only.
    pub fail_start: &'static str,
    /// A line that ends any test's output block.
    pub stop: &'static str,
    /// A package's (or file's) result line: groups `status` and `name`;
    /// empty when the runner prints none (the digest then counts no groups).
    pub group: &'static str,
    /// The `group` status words for passed and failed, `|`-separated lists
    /// (any other is "no tests").
    pub group_words: [&'static str; 2],
    /// What the digest calls the groups (`packages`, `files`).
    pub groups: &'static str,
    /// What the digest adds when tests passed but the output lists none
    /// (` (passes are listed only with -v)`).
    pub quiet: &'static str,
    /// The runner's own totals line (`Tests: 1 failed, 5 passed, 6 total`);
    /// when present its counts are the digest's. Empty: tests are counted
    /// from their result lines.
    pub totals: &'static str,
    /// On the totals line, the passed, failed and skipped counts: group
    /// `n`, summed over its matches (Surefire's `Failures: 1, Errors: 2` is
    /// one pattern); a count it does not print is 0.
    pub counts: [&'static str; 3],
    /// The runner prints a totals line per group (`dotnet test`: one per
    /// test assembly): the counts are their sum, not the last line's.
    pub sum_totals: bool,
    /// On the totals line, the count of tests run (group `n`): when set,
    /// the passed count is it less the failed and skipped counts (Maven
    /// Surefire and Gradle print no passed count). Empty: none.
    pub total: &'static str,
    /// A failure message with its location: groups `loc` and `msg` (and
    /// `line`, when the location's line number is written apart from it:
    /// `in File.cs:line 12`).
    pub location: &'static str,
    /// A panic, uncaught exception or the failure's message: group `msg`.
    pub panic: &'static str,
    /// What the digest puts before a `panic` message (`panic: `).
    pub panic_label: &'static str,
    /// A stack frame after a panic: group `loc` (and `line`, as for
    /// [`Runner::location`]).
    pub frame: &'static str,
    /// Frames of the runner and the language runtime, skipped when naming
    /// where a panic was.
    pub frame_skip: &'static str,
    /// Lines of a failure's report added to its message (jest's
    /// `Expected:` and `Received:`); empty: none.
    pub detail: &'static str,
}

/// `go test`'s row; `go test -json` is the same row over the JSON lines'
/// `Output` texts.
macro_rules! go_test_row {
    ($name:expr, $command:expr, $detect:expr, $json:expr) => {
        Runner {
            name: $name,
            command: $command,
            detect: $detect,
            json_text: $json,
            json_group: "Package",
            status: r"^\s*--- (?P<status>PASS|FAIL|SKIP): (?P<name>\S+) \(",
            words: ["PASS", "FAIL", "SKIP"],
            start: r"^=== RUN\s+(?P<name>\S+)",
            fail_start: "",
            stop: r"^\s*(?:=== |--- )|^(?:FAIL|PASS|ok|\?)(?:\s|$)",
            group: r"^(?P<status>ok|FAIL|\?)\s+(?P<name>\S+)",
            group_words: ["ok", "FAIL"],
            groups: "packages",
            quiet: " (passes are listed only with -v)",
            totals: "",
            counts: ["", "", ""],
            total: "",
            sum_totals: false,
            location: r"^\s+(?P<loc>[\w./\\-]+\.go:\d+): (?P<msg>.*)$",
            panic: r"^panic: (?P<msg>.*?)(?: \[recovered\])?$",
            panic_label: "panic: ",
            frame: r"^\t(?P<loc>\S.*\.go:\d+)",
            frame_skip: r"[/\\]src[/\\](?:testing|runtime)[/\\]",
            detail: "",
        }
    };
}

/// The test runners besides cargo (whose digest is its own). An output
/// goes to the first row its command names whose `detect` line it has,
/// else to any row whose `detect` line it has, else to the first row its
/// command names.
pub static RUNNERS: &[Runner] = &[
    go_test_row!("go test", r"\bgo\s+test\b", r"^(?:ok|FAIL)\s+\S+\s+(?:[\d.]+s|\(cached\))|^--- (?:PASS|FAIL|SKIP): \S+ \(", ""),
    go_test_row!("go test -json", r"\bgo\s+test\b.*\s-json\b", r#"^\{"Time":"[^"]*","Action":""#, "Output"),
    Runner {
        name: "jest",
        command: r"(?:^|[\s;&|/\\])jest(?:\.cmd)?(?:\s|$)",
        detect: r"^Tests:\s+.*\d+ total$",
        json_text: "",
        json_group: "",
        // Passes and skips (with --verbose); failures come from the
        // reports, which name the suite too.
        status: r"^\s+(?P<status>✓|√|○)\s+(?:skipped\s+|todo\s+)?(?P<name>.+?)(?:\s+\(\d+(?:\.\d+)?\s*m?s\))?$",
        words: ["✓|√", "✕|×", "○"],
        start: "",
        fail_start: r"^\s+● (?P<name>.+?)\s*$",
        stop: r"^\s+● |^(?:PASS|FAIL)\s|^Test Suites:|^Summary of all failing tests",
        group: r"^(?P<status>PASS|FAIL)\s+(?P<name>\S+)",
        group_words: ["PASS", "FAIL"],
        groups: "files",
        quiet: " (passes are listed only with -v)",
        totals: r"^Tests:\s+\d",
        counts: [r"(?P<n>\d+) passed", r"(?P<n>\d+) failed", r"(?P<n>\d+) skipped"],
        total: "",
        sum_totals: false,
        location: r"^\s+at (?:.*\()?(?P<loc>[^\s()]+:\d+):\d+\)?$",
        panic: r"^\s{4}(?P<msg>\S.*)$",
        panic_label: "",
        frame: r"^\s+at (?:.*\()?(?P<loc>[^\s()]+:\d+):\d+\)?$",
        frame_skip: r"node_modules|node:internal|^internal[/\\]",
        detail: r"^\s+(?:Expected|Received):\s",
    },
    Runner {
        name: "vitest",
        command: r"(?:^|[\s;&|/\\])vitest(?:\.cmd)?(?:\s|$)",
        detect: r"^\s+Test Files\s+\d+ (?:passed|failed)",
        json_text: "",
        json_group: "",
        status: r"^\s{3,}(?P<status>✓|√|↓)\s+(?P<name>.+?)(?:\s+\d+(?:\.\d+)?m?s)?$",
        words: ["✓|√", "×", "↓"],
        start: "",
        fail_start: r"^\s*FAIL\s+(?P<name>\S.*?)\s*$",
        stop: r"^⎯|^\s*FAIL\s|^\s+Test Files\s",
        group: r"^\s?(?P<status>✓|√|❯|×|↓)\s+(?P<name>\S+)\s+\(\d+ tests?",
        group_words: ["✓|√", "❯|×"],
        groups: "files",
        quiet: " (passes are listed only with -v)",
        totals: r"^\s+Tests\s+\d",
        counts: [r"(?P<n>\d+) passed", r"(?P<n>\d+) failed", r"(?P<n>\d+) skipped"],
        total: "",
        sum_totals: false,
        location: r"^\s*❯\s+(?:.*\s)?(?P<loc>\S+:\d+):\d+$",
        panic: r"^(?P<msg>[A-Za-z_$][\w$.]*: .*)$",
        panic_label: "",
        frame: r"^\s*❯\s+(?:.*\s)?(?P<loc>\S+:\d+):\d+$",
        frame_skip: r"node_modules|node:internal",
        detail: "",
    },
    Runner {
        name: "dotnet test",
        command: r"\bdotnet(?:\.exe)?\s+test\b",
        detect: r"^(?:Passed|Failed)!\s+-\s+Failed:\s+\d+|^Test Run (?:Successful|Failed)\.$",
        json_text: "",
        json_group: "",
        // Failures and skips always; passes with `--logger
        // "console;verbosity=normal"`.
        status: r"^\s{2}(?P<status>Passed|Failed|Skipped) (?P<name>\S.*?) \[[^\]]*\]$",
        words: ["Passed", "Failed", "Skipped"],
        start: "",
        fail_start: "",
        stop: r"^\s{2}(?:Passed|Failed|Skipped) \S.* \[[^\]]*\]$|^(?:Passed|Failed)!\s|^Test Run |^Total tests:|^Results File:",
        // One summary line per test assembly.
        group: r"^(?P<status>Passed|Failed)!\s+-.*\s-\s(?P<name>\S+)(?:\s+\(.*\))?$",
        group_words: ["Passed", "Failed"],
        groups: "assemblies",
        quiet: " (passes are listed only with -v)",
        totals: r"^(?:Passed|Failed)!\s+-\s+Failed:\s+\d",
        counts: [r"Passed:\s+(?P<n>\d+)", r"Failed:\s+(?P<n>\d+)", r"Skipped:\s+(?P<n>\d+)"],
        total: "",
        sum_totals: true,
        location: r"^\s+at .* in (?P<loc>.+):line (?P<line>\d+)\s*$",
        // The `Error Message:` block's first line (indented three spaces).
        panic: r"^\s{3}(?P<msg>\S.*)$",
        panic_label: "",
        frame: r"^\s+at .* in (?P<loc>.+):line (?P<line>\d+)\s*$",
        // Frames without a source file (the runtime's, the test
        // framework's) do not match `frame` at all.
        frame_skip: r"^$",
        detail: r"^(?:Expected|Actual):\s",
    },
    Runner {
        name: "pytest",
        command: r"(?:^|[\s;&|/\\])(?:py\.test|pytest)(?:\.exe)?(?:\s|$)|\bpy(?:thon[\d.]*)?(?:\.exe)?\s+-m\s+pytest\b",
        detect: r"^=+ test session starts =+$|^=+ .*\bin [\d.]+s(?: \([\d:.]+\))? =+$",
        json_text: "",
        json_group: "",
        // Passes and skips with -v (`path::test PASSED [ 50%]`); failures
        // come from the reports under `FAILURES` and `ERRORS`, named as
        // their headers name them.
        status: r"^(?P<name>\S+::\S.*?) (?P<status>PASSED|SKIPPED|XFAIL|XPASS)(?:\s|$)",
        words: ["PASSED|XPASS", "", "SKIPPED|XFAIL"],
        start: "",
        fail_start: r"^_{3,} (?P<name>\S.*?) _{3,}$",
        stop: r"^_{3,} \S.* _{3,}$|^={3,} ",
        // Its per-file progress lines wrap without the file's name: no
        // groups are counted.
        group: "",
        group_words: ["", ""],
        groups: "files",
        quiet: " (passes are listed only with -v)",
        totals: r"^=+ .*\b(?:passed|failed|skipped|errors?|no tests ran)\b.* in [\d.]+s",
        counts: [r"(?P<n>\d+) passed", r"(?P<n>\d+) failed", r"(?P<n>\d+) skipped"],
        total: "",
        sum_totals: false,
        location: r"^(?P<loc>(?:[A-Za-z]:)?[^\s:]+\.py:\d+): (?P<msg>\S.*)$",
        // The `E` lines of a report: the first is the exception or the
        // failed assertion.
        panic: r"^E\s+(?P<msg>\S.*)$",
        panic_label: "",
        frame: r"^(?P<loc>(?:[A-Za-z]:)?[^\s:]+\.py:\d+):(?:\s|$)",
        frame_skip: r"[/\\](?:site-packages|_pytest|pluggy)[/\\]",
        detail: r"^E\s+\+\s+where\s",
    },
    Runner {
        name: "mvn test",
        // Any Maven build that runs Surefire (`test`, `verify`, `install`).
        command: r"(?:^|[\s;&|/\\])(?:mvnw?|mvnd)(?:\.cmd|\.bat)?(?:\s|$)",
        detect: r"^\[(?:INFO|ERROR|WARNING)\] Tests run: \d+, Failures: \d+, Errors: \d+, Skipped: \d+",
        json_text: "",
        json_group: "",
        // Surefire lists failed tests only (`<<< FAILURE!` for an assertion,
        // `<<< ERROR!` for an exception), Surefire 3's form and the older
        // `method(Class)` one; the exception and its frames follow.
        status: r"^(?:\[ERROR\] )?(?P<name>[\w$.]+(?:\([\w$.]+\))?)\s+(?:--\s+)?Time elapsed: [\d.,]+ s(?:ec)?\s+<<< (?P<status>FAILURE|ERROR)!$",
        words: ["", "FAILURE|ERROR", ""],
        start: "",
        fail_start: "",
        stop: r"^\s*$|^\[(?:INFO|ERROR|WARNING)\]",
        // A test class's line.
        group: r"^\[(?P<status>INFO|WARNING|ERROR)\] Tests run: \d+, .*\s-+ in (?P<name>\S+)$",
        group_words: ["INFO|WARNING", "ERROR"],
        groups: "classes",
        quiet: " (Surefire lists failures only)",
        // One summary per module.
        totals: r"^\[(?:INFO|ERROR|WARNING)\] Tests run: \d+, Failures: \d+, Errors: \d+, Skipped: \d+$",
        counts: ["", r"(?:Failures|Errors): (?P<n>\d+)", r"Skipped: (?P<n>\d+)"],
        sum_totals: true,
        total: r"Tests run: (?P<n>\d+)",
        location: r"^\[ERROR\]\s+(?P<loc>[\w$.]+:\d+) (?P<msg>.*)$",
        panic: r"^(?P<msg>[\w$.]+(?:Exception|Error|Failure|Throwable)\b.*)$",
        panic_label: "",
        frame: r"^\s+at [\w$.<>]+\((?P<loc>[^():]+\.(?:java|kt|scala|groovy):\d+)\)$",
        frame_skip: r"^(?:Assert|Assertions|AssertEquals|AssertionFailureBuilder|Method|ArrayList|Enum|NativeMethodAccessorImpl)\.java:",
        detail: "",
    },
    Runner {
        name: "gradle test",
        command: r"(?:^|[\s;&|/\\])gradlew?(?:\.bat)?(?:\s|$)",
        detect: r"^\d+ tests? completed, \d+ failed|^> Task :(?:\S+:)?(?:test|\w+Test)(?: FAILED)?$",
        json_text: "",
        json_group: "",
        // Failures always (`Class > method() FAILED`), passes and skips
        // when the build's test logging asks for them.
        status: r"^(?P<name>\S.*? > .*?) (?P<status>PASSED|FAILED|SKIPPED)$",
        words: ["PASSED", "FAILED", "SKIPPED"],
        start: "",
        fail_start: "",
        stop: r"^\S|^\s*$",
        // A test task's line.
        group: r"^> Task (?P<name>:(?:\S+:)?(?:test|\w+Test))(?: (?P<status>FAILED|UP-TO-DATE|FROM-CACHE|NO-SOURCE|SKIPPED))?$",
        group_words: ["|UP-TO-DATE|FROM-CACHE", "FAILED"],
        groups: "tasks",
        quiet: " (Gradle lists failures only)",
        // One line per failed test task; a passing task prints none.
        totals: r"^\d+ tests? completed, ",
        counts: ["", r"(?P<n>\d+) failed", r"(?P<n>\d+) skipped"],
        sum_totals: true,
        total: r"(?P<n>\d+) tests? completed",
        // The short exception format: `    java.lang.X at FooTest.java:26`.
        location: r"^\s+(?P<msg>[\w$.]+) at (?P<loc>[\w$.-]+\.(?:java|kt|scala|groovy):\d+)$",
        // The full format (`exceptionFormat = "full"`): the exception with
        // its message, then its frames.
        panic: r"^\s{4}(?P<msg>[\w$.]+(?:Exception|Error|Failure|Throwable)(?::.*)?)$",
        panic_label: "",
        frame: r"^\s+at [\w$.<>]+\((?P<loc>[^():]+\.(?:java|kt|scala|groovy):\d+)\)$",
        frame_skip: r"^(?:Assert|Assertions|AssertEquals|AssertionFailureBuilder|Method|ArrayList|Enum|NativeMethodAccessorImpl)\.java:",
        detail: "",
    },
];

/// True when `word` is one of a `|`-separated list.
fn one_of(list: &str, word: &str) -> bool {
    list.split('|').any(|w| w == word)
}

/// A line without terminal colour codes (`ESC [ ... letter`).
fn plain(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                while let Some(&n) = it.peek() {
                    it.next();
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    std::borrow::Cow::Owned(out)
}

/// The runner a command or an output is from, if any row matches.
fn runner_of(cmd: &str, text: &str) -> Option<&'static Runner> {
    let has = |r: &Runner| {
        let d = re(r.detect);
        text.lines().take(5000).any(|l| d.is_match(&plain(l)))
    };
    let by_cmd: Vec<&'static Runner> = RUNNERS.iter().filter(|r| re(r.command).is_match(cmd)).collect();
    // The output decides between rows the command names (or overrules
    // it: `go test` with `GOFLAGS=-json` in the environment).
    by_cmd.iter().find(|r| has(r)).copied().or_else(|| RUNNERS.iter().find(|r| has(r))).or(by_cmd.first().copied())
}

/// The text a runner's patterns read: colour codes out, and for a
/// JSON-lines runner the text field of each line (a line that is not JSON
/// is kept as it is). With it, for each of its lines, the line of `text`
/// it came from (1-based).
fn runner_text(r: &Runner, text: &str) -> (String, Vec<usize>) {
    /// One group's text so far (a package's), its lines' origins, and
    /// where its unfinished line began.
    #[derive(Default)]
    struct Stream {
        text: String,
        map: Vec<usize>,
        partial: Option<usize>,
    }
    impl Stream {
        fn push(&mut self, piece: &str, at: usize) {
            for seg in piece.split_inclusive('\n') {
                self.partial.get_or_insert(at);
                self.text.push_str(seg.trim_end_matches('\n'));
                if seg.ends_with('\n') {
                    self.end_line();
                }
            }
        }
        fn end_line(&mut self) {
            if let Some(at) = self.partial.take() {
                self.text.push('\n');
                self.map.push(at);
            }
        }
    }
    // Streams by group, in the order each group first appears (a JSON
    // runner's packages run in parallel and interleave their lines).
    let mut streams: Vec<(String, Stream)> = Vec::new();
    let mut stream = |key: &str| -> usize {
        match streams.iter().position(|(k, _)| k == key) {
            Some(i) => i,
            None => {
                streams.push((key.to_string(), Stream::default()));
                streams.len() - 1
            }
        }
    };
    let mut order: Vec<(usize, String, usize)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = plain(line);
        if !r.json_text.is_empty() && line.starts_with('{') {
            let Ok(v) = crate::json::parse(&line) else { continue };
            let Some(piece) = v.get(r.json_text).and_then(|t| t.as_str()) else { continue };
            let key = if r.json_group.is_empty() { "" } else { v.get(r.json_group).and_then(|g| g.as_str()).unwrap_or("") };
            order.push((stream(key), piece.to_string(), i + 1));
        } else {
            order.push((stream(""), format!("{line}\n"), i + 1));
        }
    }
    let mut all: Vec<Stream> = streams.into_iter().map(|_| Stream::default()).collect();
    for (s, piece, at) in order {
        all[s].push(&piece, at);
    }
    let mut out = String::with_capacity(text.len());
    let mut map = Vec::new();
    for mut s in all {
        s.end_line();
        out.push_str(&s.text);
        map.extend(s.map);
    }
    (out, map)
}

/// The end (exclusive) of the block starting at line `i`: the next stop
/// line, trailing blank lines left out.
fn block_end(lines: &[&str], i: usize, stop: &Regex) -> usize {
    let mut j = i + 1;
    while j < lines.len() && !stop.is_match(lines[j]) {
        j += 1;
    }
    while j > i + 1 && lines[j - 1].trim().is_empty() {
        j -= 1;
    }
    j
}

/// A runner's items: each test result line (`ok`, `failed`, `ignored`),
/// each failing test's output (`failed-output`: the lines after its result
/// line, those after its start line, or its report), and each package or
/// file line (`package ok`, `package failed`, `package no tests`). Lines
/// are those of `text` (for a JSON-lines runner, the JSON lines).
pub fn runner_items(r: &Runner, text: &str) -> Vec<Item> {
    let (plain_text, map) = runner_text(r, text);
    let at = |n: usize| map.get(n - 1).copied().unwrap_or(n);
    let mut v = items_of(r, &plain_text);
    for it in &mut v {
        // Regrouped by package, a block's lines need not be in the order of
        // the JSON lines (a block can run on into another package's lines,
        // which came earlier): it ends at the last JSON line of any of its
        // lines, never before its first.
        let first = at(it.first);
        it.last = (it.first..=it.last).map(at).max().unwrap_or(first).max(first);
        it.first = first;
    }
    v
}

/// [`runner_items`] over the text the patterns read ([`runner_text`]), in
/// its own line numbers.
fn items_of(r: &Runner, text: &str) -> Vec<Item> {
    let lines: Vec<&str> = text.lines().collect();
    let (status, stop) = (re(r.status), re(r.stop));
    let group = (!r.group.is_empty()).then(|| re(r.group));
    let start = (!r.start.is_empty()).then(|| re(r.start));
    let fail_start = (!r.fail_start.is_empty()).then(|| re(r.fail_start));
    let mut v = Vec::new();
    let mut runs: Vec<(String, usize, usize)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some(c) = status.captures(l) {
            let name = c.name("name").map_or("", |m| m.as_str()).to_string();
            let word = c.name("status").map_or("", |m| m.as_str());
            let st = if one_of(r.words[1], word) {
                "failed"
            } else if one_of(r.words[2], word) {
                "ignored"
            } else {
                "ok"
            };
            v.push(Item { name: name.clone(), first: i + 1, last: i + 1, status: st.into(), depth: 0 });
            let j = block_end(&lines, i, &stop);
            if st == "failed" && j > i + 1 {
                v.push(Item { name, first: i + 1, last: j, status: "failed-output".into(), depth: 0 });
            }
        } else if let Some(c) = fail_start.as_ref().and_then(|s| s.captures(l)) {
            let name = c.name("name").map_or("", |m| m.as_str()).to_string();
            let j = block_end(&lines, i, &stop);
            v.push(Item { name: name.clone(), first: i + 1, last: i + 1, status: "failed".into(), depth: 0 });
            v.push(Item { name, first: i + 1, last: j, status: "failed-output".into(), depth: 0 });
        } else if let Some(c) = start.as_ref().and_then(|s| s.captures(l)) {
            let j = block_end(&lines, i, &stop);
            if j > i + 1 {
                runs.push((c.name("name").map_or("", |m| m.as_str()).to_string(), i + 1, j));
            }
        } else if let Some(c) = group.as_ref().and_then(|g| g.captures(l)) {
            let word = c.name("status").map_or("", |m| m.as_str());
            let st = if one_of(r.group_words[0], word) {
                "package ok"
            } else if one_of(r.group_words[1], word) {
                "package failed"
            } else {
                "package no tests"
            };
            v.push(Item { name: c.name("name").map_or("", |m| m.as_str()).to_string(), first: i + 1, last: i + 1, status: st.into(), depth: 0 });
        }
    }
    let failed: Vec<String> = v.iter().filter(|i| i.status == "failed").map(|i| i.name.clone()).collect();
    for (name, first, last) in runs {
        if failed.contains(&name) {
            v.push(Item { name, first, last, status: "failed-output".into(), depth: 0 });
        }
    }
    v.sort_by_key(|i| i.first);
    v
}

/// Why a failure's output says it failed: `location message`, or a
/// panic's (an exception's, a failure message's) first frame outside the
/// runner and its message, with the row's detail lines.
fn runner_why(r: &Runner, block: &[&str]) -> String {
    let (loc, panic, frame, skip) = (re(r.location), re(r.panic), re(r.frame), re(r.frame_skip));
    let detail = (!r.detail.is_empty()).then(|| re(r.detail));
    // A report's first line names the test; the message is below it.
    let body = if r.fail_start.is_empty() { block } else { block.get(1..).unwrap_or(&[]) };
    for (k, l) in body.iter().enumerate() {
        if let Some(c) = panic.captures(l) {
            let at = body[k + 1..].iter().filter_map(|f| frame.captures(f)).map(|c| loc_of(&c)).find(|f| !skip.is_match(f));
            let mut msg = format!("{}{}", r.panic_label, c.name("msg").map_or("", |m| m.as_str()).trim());
            if let Some(d) = &detail {
                for x in body[k + 1..].iter().filter(|x| d.is_match(x)).take(2) {
                    msg.push_str("; ");
                    msg.push_str(x.trim());
                }
            }
            let msg = clip(&msg, QUOTE);
            return match at {
                Some(at) => format!("{} {msg}", at.replace('\\', "/")),
                None => msg,
            };
        }
    }
    body.iter()
        .find_map(|l| loc.captures(l))
        .map(|c| format!("{} {}", loc_of(&c).replace('\\', "/"), clip(c.name("msg").map_or("", |m| m.as_str()).trim(), QUOTE)))
        .unwrap_or_default()
}

/// A location match as `path:line`: its `loc` group, with its `line`
/// group added when the runner writes the line apart.
fn loc_of(c: &crate::regex::Captures<'_>) -> String {
    let loc = c.name("loc").map_or("", |m| m.as_str());
    match c.name("line") {
        Some(l) => format!("{loc}:{}", l.as_str()),
        None => loc.to_string(),
    }
}

fn runner_test(r: &Runner, id: &str, raw: &str, exit: Option<i32>, max: usize) -> String {
    let (text, _) = runner_text(r, raw);
    let items = items_of(r, &text);
    let lines: Vec<&str> = text.lines().collect();
    let count = |s: &str| items.iter().filter(|i| i.status == s).count();
    let mut failed: Vec<&Item> = Vec::new();
    for it in items.iter().filter(|i| i.status == "failed") {
        // A parent fails because a child did: the child says why.
        let parent = items.iter().any(|o| o.status == "failed" && o.name.len() > it.name.len() + 1 && o.name.starts_with(&it.name) && o.name.as_bytes()[it.name.len()] == b'/');
        if !parent && !failed.iter().any(|f| f.name == it.name) {
            failed.push(it);
        }
    }
    // A report printed twice (jest's summary of failing tests) is one
    // failure.
    let distinct_failed = {
        let mut names: Vec<&str> = items.iter().filter(|i| i.status == "failed").map(|i| i.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        names.len()
    };
    let (mut passed, mut failed_n, mut skipped) = (count("ok"), distinct_failed, count("ignored"));
    // The runner's own totals line, when it prints one, is the count.
    let totals = (!r.totals.is_empty()).then(|| re(r.totals));
    if let Some(t) = totals {
        let n = |p: &str, l: &str| {
            if p.is_empty() {
                return 0;
            }
            re(p).captures_iter(l).filter_map(|c| c.name("n").and_then(|m| m.as_str().parse::<usize>().ok())).sum::<usize>()
        };
        let found: Vec<&&str> = if r.sum_totals {
            lines.iter().filter(|l| t.is_match(l)).collect()
        } else {
            lines.iter().rev().find(|l| t.is_match(l)).into_iter().collect()
        };
        if !found.is_empty() {
            let sum = |k: usize| found.iter().map(|l| n(r.counts[k], l)).sum::<usize>();
            (passed, failed_n, skipped) = (sum(0), sum(1), sum(2));
            if !r.total.is_empty() {
                let run: usize = found.iter().map(|l| n(r.total, l)).sum();
                passed = run.saturating_sub(failed_n + skipped);
            }
        }
    }
    let (pk_ok, pk_failed, pk_none) = (count("package ok"), count("package failed"), count("package no tests"));
    let verdict = match exit {
        Some(0) => "passed",
        Some(_) => "failed",
        None if failed_n > 0 || pk_failed > 0 => "exit status unknown (inferred: failed)",
        None if pk_ok > 0 || passed > 0 => "exit status unknown (inferred: passed)",
        None => "exit status unknown (no result line)",
    };
    // Without -v a passing test prints nothing.
    let quiet = passed == 0 && pk_ok > 0 && (r.start.is_empty() || !lines.iter().any(|l| re(r.start).is_match(l)));
    // A runner without group lines (pytest) says nothing of groups.
    let groups =
        if r.group.is_empty() { String::new() } else { format!("; {} {pk_ok} ok {pk_failed} failed {pk_none} without tests", r.groups) };
    let mut out = format!(
        "{}: tests {passed} passed {failed_n} failed {skipped} skipped{}{groups}: {verdict}\n",
        r.name,
        if quiet { r.quiet } else { "" },
    );
    for f in failed.iter().take(MAX_FAILURES) {
        let why = items.iter().filter(|i| i.status == "failed-output" && i.name == f.name).map(|b| runner_why(r, &lines[b.first - 1..b.last])).find(|w| !w.is_empty()).unwrap_or_default();
        out.push_str(&format!("FAILED {} {}\n", f.name, why).replace(" \n", "\n"));
    }
    if failed.len() > MAX_FAILURES {
        out.push_str(&format!("and {} more failed\n", failed.len() - MAX_FAILURES));
    }
    if exit != Some(0) {
        // Pass or fail is a guess without an exit code: the verbatim tail
        // keeps a wrong guess from hiding a failure.
        out.push_str(&tail(&text, max.saturating_sub(out.len()).max(200)));
    }
    out.push_str(&hint(id, "--failures | --item NAME | --lines A-B | --grep RE"));
    out
}

// ---- cargo build ---------------------------------------------------------

fn build_items(text: &str) -> Vec<Item> {
    let lines: Vec<&str> = text.lines().collect();
    let mut v = Vec::new();
    let (mut ne, mut nw) = (0, 0);
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if build_head(l).is_some() && !build_skip(l) {
            let mut j = i + 1;
            loop {
                while j < lines.len() && !lines[j].trim().is_empty() && build_head(lines[j]).is_none() {
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
    block
        .iter()
        .find_map(|l| {
            let c = build_at(l)?;
            let g = |c: lex::Span| c.map_or("", |(a, b)| &l[a..b]);
            Some(format!("{}:{}", g(c[1]).replace('\\', "/"), g(c[2])))
        })
        .unwrap_or_default()
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
    // The code (`[E0308]`) and message of a message's first line.
    let head =
        |l: &str| build_head(l).map(|c| (c[2].map_or("", |(a, b)| &l[a..b]).to_string(), c[3].map_or("", |(a, b)| &l[a..b]).to_string()));
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
        let (code, msg) = head(b[0]).unwrap_or_default();
        out.push_str(&format!("{} error{code} {} {}\n", e.name, location(b), clip(&msg, QUOTE)));
    }
    if errors.len() > MAX_FAILURES {
        out.push_str(&format!("and {} more errors\n", errors.len() - MAX_FAILURES));
    }
    if !warnings.is_empty() {
        let mut groups: Vec<(String, usize)> = Vec::new();
        for w in &warnings {
            let msg = head(lines[w.first - 1]).map(|c| lint_key(&c.1)).unwrap_or_default();
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
    match lang {
        Lang::Markdown => return md_outline(&lines),
        Lang::Python => return py_outline(&lines),
        Lang::Brace => return code_outline(&lines),
        Lang::Auto => {}
    }
    let headings = lines.iter().filter(|l| md_heading(l).is_some()).count();
    if headings >= 2 && headings * 4 >= lines.iter().filter(|l| l.starts_with("fn ") || l.starts_with("pub ")).count() {
        return md_outline(&lines);
    }
    let pys = lines.iter().filter(|l| py_def(l).is_some()).count();
    let braces = text.bytes().filter(|b| *b == b'{').count();
    if pys > 0 && braces < pys {
        return py_outline(&lines);
    }
    code_outline(&lines)
}

fn md_outline(lines: &[&str]) -> Vec<Item> {
    let mut v: Vec<Item> = Vec::new();
    let mut fence = false;
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("```") {
            fence = !fence;
        }
        if fence {
            continue;
        }
        if let Some(c) = md_heading(l) {
            let (hashes, text) = (c[1].unwrap_or((0, 0)), c[2].unwrap_or((0, 0)));
            v.push(Item {
                name: l[text.0..text.1].to_string(),
                first: i + 1,
                last: lines.len(),
                status: "heading".into(),
                depth: hashes.1 - hashes.0 - 1,
            });
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

fn py_outline(lines: &[&str]) -> Vec<Item> {
    let mut v: Vec<Item> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(c) = py_def(l) else { continue };
        let g = |c: lex::Span| c.map_or("", |(a, b)| &l[a..b]);
        let indent = g(c[1]).len();
        while stack.last().is_some_and(|s| s.0 >= indent) {
            stack.pop();
        }
        let kind = if g(c[2]) == "class" { "class" } else { "def" };
        let name = match stack.last() {
            Some((_, p)) => format!("{p}.{}", g(c[3])),
            None => g(c[3]).to_string(),
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
    brace_end(lines, start).0
}

/// Like [`item_end`], but only when the item has a brace body that closes:
/// `None` for a declaration without one (a field, a Python `def`).
pub fn brace_body_end(lines: &[&str], start: usize) -> Option<usize> {
    let (end, body) = brace_end(lines, start);
    body.then_some(end)
}

/// The line where the item starting at `start` ends, and whether that is
/// the close of a brace body: the line that closes its first brace, the line of a `;` or `,` reached before any brace (a
/// field or a variant), or the line before a `}` that closes its container.
/// Strings, chars and comments are skipped roughly.
fn brace_end(lines: &[&str], start: usize) -> (usize, bool) {
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
                            return (i + 1, true);
                        }
                        if !opened && depth < 0 {
                            // The container closed: the item ended before it.
                            return (i.max(start + 1), false);
                        }
                    }
                    b';' if !opened && depth == 0 => return (i + 1, false),
                    b'(' | b'[' => nest += 1,
                    b')' | b']' => nest -= 1,
                    b'<' if !opened => nest += 1,
                    b'>' if !opened && k > 0 && b[k - 1] != b'-' && b[k - 1] != b'=' => nest -= 1,
                    b',' if !opened && depth == 0 && nest <= 0 => return (i + 1, false),
                    _ => {}
                }
            }
            k += 1;
        }
    }
    (lines.len(), false)
}

fn code_outline(lines: &[&str]) -> Vec<Item> {
    let mut v: Vec<Item> = Vec::new();
    // Open containers: (end line, indent, name).
    let mut stack: Vec<(usize, usize, String)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(c) = code_decl(l) else { continue };
        let g = |c: lex::Span| c.map_or("", |(a, b)| &l[a..b]);
        let indent = g(c[1]).len();
        if indent > 8 {
            continue;
        }
        while stack.last().is_some_and(|s| s.0 < i + 1) {
            stack.pop();
        }
        let kind = g(c[2]).trim_end_matches('!').to_string();
        let rest = g(c[3]);
        let name = if kind == "impl" {
            // `impl<T> Trait for Type {` names the impl by its type.
            let r = rest.trim_start();
            let r = if r.starts_with('<') { r.find('>').map_or(r, |k| &r[k + 1..]) } else { r };
            let r = r.split(" where").next().unwrap_or(r).trim_end_matches('{').trim();
            let ty = r.rsplit(" for ").next().unwrap_or(r).trim();
            ty.split(['<', ' ']).next().unwrap_or(ty).to_string()
        } else {
            let r = rest.trim_start();
            outline_ident(r).and_then(|m| m[1]).map(|(a, b)| r[a..b].to_string()).unwrap_or_default()
        };
        if name.is_empty() {
            continue;
        }
        let end = brace_end(lines, i).0;
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
    let mut out = String::new();
    let mut i = 0;
    let mut tables = 0;
    while i < lines.len() {
        if table_head(lines[i]) {
            let end = (i + 1..lines.len()).find(|&j| lines[j].trim_start().starts_with("total")).unwrap_or(lines.len() - 1);
            if i > 0 && !lines[i - 1].trim().is_empty() && lines[i - 1].len() <= 80 && !table_head(lines[i - 1]) {
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
        if summary_fail(t) && !t.starts_with("error: could not compile") && !t.starts_with("error: test failed") {
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
    for l in lines.iter().filter(|l| verdict_line(l.trim_start())) {
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
        if crate::sources::path_denied(std::path::Path::new(&file)) {
            return Err(crate::sources::denied_message(std::path::Path::new(&file)));
        }
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
    let hits: Vec<(usize, &str)> = text.lines().enumerate().filter(|(_, l)| bad_word(l)).collect();
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
    /// Real `go test` outputs of a small module (fixtures/go-test): three
    /// packages, one with failing tests and subtests, a skip, a panic and a
    /// build failure; paths outside the module rewritten.
    fn go_out(name: &str) -> String {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/go-test").join(name)).unwrap()
    }

    #[test]
    fn go_test_is_recognised_by_command_and_by_text() {
        let go = &RUNNERS[0];
        assert_eq!(shape("bash", "cd svc && go test ./...", ""), Shape::Test(go));
        assert_eq!(shape("bash", "go test -v -run TestAdd ./calc", ""), Shape::Test(go));
        for f in ["plain.txt", "verbose.txt", "panic.txt", "pass.txt", "build.txt"] {
            assert_eq!(shape("run", "", &go_out(f)), Shape::Test(go), "{f}");
        }
        assert_eq!(shape("bash", "cargo test", ""), Shape::CargoTest, "cargo keeps its own digest");
        assert_eq!(shape("bash", "make", "ok then
all good
"), Shape::Generic);
    }

    #[test]
    fn go_test_digest_names_each_failing_leaf_with_its_location() {
        let go = Shape::Test(&RUNNERS[0]);
        let d = digest("h:1", "go test ./...", go, &go_out("plain.txt"), Some(1), 600);
        let head: Vec<&str> = d.lines().skip(1).take(4).collect();
        assert_eq!(
            head,
            [
                "go test: tests 0 passed 4 failed 0 skipped (passes are listed only with -v); packages 1 ok 1 failed 1 without tests: failed",
                "FAILED TestAdd calc_test.go:7 Add(1, 2) = 4, want 3",
                "FAILED TestTable/zero calc_test.go:15 got 1, want 0",
                "FAILED TestTable/one calc_test.go:15 got 2, want 1",
            ]
        );
        assert!(d.ends_with("atlas slice h:1 --failures | --item NAME | --lines A-B | --grep RE
"), "{d}");
        // -v prints each message before the result line: the same failures.
        let v = digest("h:2", "go test -v ./...", go, &go_out("verbose.txt"), Some(1), 600);
        assert!(v.contains("go test: tests 2 passed 4 failed 1 skipped; packages 1 ok 1 failed 1 without tests: failed
"), "{v}");
        for l in &head[1..] {
            assert!(v.contains(l), "{l} in {v}");
        }
        // A panic names its first frame outside the runtime.
        let p = digest("h:3", "go test ./...", go, &go_out("panic.txt"), None, 600);
        assert!(p.contains("FAILED TestDivZero /home/dev/demo/calc/calc.go:7 panic: runtime error: integer divide by zero
"), "{p}");
        assert!(p.contains(": exit status unknown (inferred: failed)
"), "{p}");
    }

    #[test]
    fn go_test_passing_and_build_failures() {
        let go = Shape::Test(&RUNNERS[0]);
        let d = digest("h:1", "go test ./...", go, &go_out("pass.txt"), Some(0), 600);
        assert_eq!(d.lines().nth(1), Some("go test: tests 0 passed 0 failed 0 skipped (passes are listed only with -v); packages 2 ok 0 failed 1 without tests: passed"));
        assert!(!d.contains("last "), "no tail on a pass: {d}");
        let d = digest("h:1", "go test -v ./...", go, &go_out("passv.txt"), None, 600);
        assert_eq!(d.lines().nth(1), Some("go test: tests 6 passed 0 failed 1 skipped; packages 2 ok 0 failed 1 without tests: exit status unknown (inferred: passed)"));
        let d = digest("h:1", "go test ./...", go, &go_out("build.txt"), Some(1), 600);
        assert!(d.contains("packages 1 ok 1 failed 1 without tests: failed"), "{d}");
        assert!(d.contains("undefined: Missing"), "the tail keeps the compiler error: {d}");
    }

    #[test]
    fn go_test_items_slice_failures_and_packages() {
        let text = go_out("verbose.txt");
        let items = items(Shape::Test(&RUNNERS[0]), &text);
        let lines: Vec<&str> = text.lines().collect();
        let blocks: Vec<String> = items.iter().filter(|i| i.status == "failed-output").map(|i| format!("{} {}", i.name, lines[i.first - 1..i.last].join("|"))).collect();
        assert_eq!(
            blocks,
            [
                "TestAdd === RUN   TestAdd|    calc_test.go:7: Add(1, 2) = 4, want 3",
                "TestTable/zero === RUN   TestTable/zero|    calc_test.go:15: got 1, want 0",
                "TestTable/one === RUN   TestTable/one|    calc_test.go:15: got 2, want 1",
            ]
        );
        let pk: Vec<(&str, &str)> = items.iter().filter(|i| i.status.starts_with("package")).map(|i| (i.name.as_str(), i.status.as_str())).collect();
        assert_eq!(pk, [("example.com/demo/calc", "package failed"), ("example.com/demo/empty", "package no tests"), ("example.com/demo/shapes", "package ok")]);
    }
}
/// The JSON-lines, jest, vitest and `dotnet test` rows of [`RUNNERS`], on
/// real outputs of small demo projects (fixtures/go-test/json-*.txt,
/// fixtures/js-test, fixtures/dotnet-test, fixtures/py-test):
/// colour codes as the runners write them on Windows, paths rewritten.
#[cfg(test)]
mod runner_rows {
    use super::*;

    fn out(dir: &str, name: &str) -> String {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(dir).join(name)).unwrap()
    }

    fn row(name: &str) -> &'static Runner {
        RUNNERS.iter().find(|r| r.name == name).unwrap()
    }

    /// The digest's lines after its header, up to the tail.
    fn head(cmd: &str, text: &str, exit: Option<i32>) -> Vec<String> {
        let d = digest("h:1", cmd, shape("bash", cmd, text), text, exit, 600);
        d.lines().skip(1).take_while(|l| !l.starts_with("last ") && !l.starts_with("atlas slice")).map(str::to_string).collect()
    }

    #[test]
    fn each_row_is_recognised_by_command_and_by_text() {
        for (cmd, dir, file, want) in [
            ("go test -json ./...", "go-test", "json-fail.txt", "go test -json"),
            ("cd svc && go test ./...", "go-test", "json-fail.txt", "go test -json"),
            ("", "go-test", "json-pass.txt", "go test -json"),
            ("go test ./...", "go-test", "plain.txt", "go test"),
            ("npx jest", "js-test", "jest-fail.txt", "jest"),
            ("", "js-test", "jest-fail-verbose.txt", "jest"),
            ("npx vitest run", "js-test", "vitest-fail.txt", "vitest"),
            ("", "js-test", "vitest-pass.txt", "vitest"),
            ("dotnet test", "dotnet-test", "fail.txt", "dotnet test"),
            ("cd tests && dotnet.exe test --no-build", "dotnet-test", "pass-normal.txt", "dotnet test"),
            ("", "dotnet-test", "sln-fail.txt", "dotnet test"),
            ("python -m pytest", "py-test", "fail.txt", "pytest"),
            ("uv run pytest -v", "py-test", "fail-verbose.txt", "pytest"),
            ("", "py-test", "pass.txt", "pytest"),
            ("mvn -B test", "mvn-test", "fail.txt", "mvn test"),
            ("./mvnw verify", "mvn-test", "pass.txt", "mvn test"),
            ("", "mvn-test", "fail.txt", "mvn test"),
            ("./gradlew test --console=plain", "gradle-test", "fail.txt", "gradle test"),
            ("gradle.bat test", "gradle-test", "pass.txt", "gradle test"),
            ("", "gradle-test", "fail.txt", "gradle test"),
        ] {
            let text = out(dir, file);
            assert_eq!(shape("bash", cmd, &text), Shape::Test(row(want)), "{cmd:?} {file}");
        }
        assert_eq!(
            shape("bash", "cat jest.config.js", "module.exports = {};\n"),
            Shape::Source(Lang::Brace),
            "a file named jest is not a run"
        );
    }

    #[test]
    fn dotnet_test_sums_its_assemblies_and_names_the_failing_frame() {
        let d = |file: &str, exit: Option<i32>| head("dotnet test", &out("dotnet-test", file), exit);
        let failures = [
            "FAILED Calc.Tests.CalculatorTests.DividesEvenly C:/src/demo/Calc/Calculator.cs:9 System.DivideByZeroException : Attempted to divide by zero.",
            "FAILED Calc.Tests.CalculatorTests.SubtractsTwoNumbers C:/src/demo/Calc.Tests/CalculatorTests.cs:17 Assert.Equal() Failure: Values differ; Expected: 1; Actual:   5",
        ];
        let want = |head: &str| std::iter::once(head.to_string()).chain(failures.iter().map(|f| f.to_string())).collect::<Vec<_>>();
        assert_eq!(
            d("fail.txt", Some(1)),
            want("dotnet test: tests 4 passed 2 failed 1 skipped; assemblies 0 ok 1 failed 0 without tests: failed")
        );
        // The summary of each test assembly is summed (a solution's run).
        assert_eq!(
            d("sln-fail.txt", Some(1)),
            want("dotnet test: tests 5 passed 2 failed 1 skipped; assemblies 1 ok 1 failed 0 without tests: failed")
        );
        // With the console logger's normal verbosity there is no summary
        // line: the result lines are counted.
        assert_eq!(
            d("fail-normal.txt", Some(1)),
            want("dotnet test: tests 4 passed 2 failed 1 skipped; assemblies 0 ok 0 failed 0 without tests: failed")
        );
        assert_eq!(
            d("pass.txt", Some(0)),
            ["dotnet test: tests 3 passed 0 failed 0 skipped; assemblies 1 ok 0 failed 0 without tests: passed"]
        );
        assert_eq!(
            d("pass-normal.txt", Some(0)),
            ["dotnet test: tests 3 passed 0 failed 0 skipped; assemblies 0 ok 0 failed 0 without tests: passed"]
        );
        let items = items(Shape::Test(row("dotnet test")), &out("dotnet-test", "sln-fail.txt"));
        let failed: Vec<(&str, usize, usize)> =
            items.iter().filter(|i| i.status == "failed-output").map(|i| (i.name.as_str(), i.first, i.last)).collect();
        assert_eq!(
            failed,
            [("Calc.Tests.CalculatorTests.DividesEvenly", 21, 28), ("Calc.Tests.CalculatorTests.SubtractsTwoNumbers", 29, 37)]
        );
        let groups: Vec<(&str, &str)> =
            items.iter().filter(|i| i.status.starts_with("package")).map(|i| (i.name.as_str(), i.status.as_str())).collect();
        assert_eq!(groups, [("Calc.Tests.dll", "package failed"), ("Calc.More.dll", "package ok")]);
    }

    #[test]
    fn go_test_json_reads_the_output_texts_package_by_package() {
        let text = out("go-test", "json-fail.txt");
        assert_eq!(
            head("go test -json ./...", &text, Some(1)),
            [
                "go test -json: tests 3 passed 4 failed 1 skipped; packages 1 ok 2 failed 1 without tests: failed",
                "FAILED TestAdd calc_test.go:7 Add(1, 2) = 4, want 3",
                "FAILED TestTable/zero calc_test.go:15 got 1, want 0",
                "FAILED TestIndex /home/dev/demo/boom/boom.go:3 panic: runtime error: index out of range [5] with length 1",
            ]
        );
        // Items point at the JSON lines: the panicking test's output runs
        // from its result to its stack's last frame, though another
        // package's lines come in between.
        let items = items(Shape::Test(row("go test -json")), &text);
        let lines: Vec<&str> = text.lines().collect();
        let b = items.iter().find(|i| i.status == "failed-output" && i.name == "TestIndex").unwrap();
        assert!(lines[b.first - 1].contains("--- FAIL: TestIndex"), "{}", lines[b.first - 1]);
        assert!(lines[b.last - 1].contains("testing.go:1851"), "{}", lines[b.last - 1]);
        assert_eq!(
            head("go test -json ./shapes/...", &out("go-test", "json-pass.txt"), Some(0)),
            ["go test -json: tests 1 passed 0 failed 1 skipped; packages 1 ok 0 failed 0 without tests: passed"]
        );
    }

    /// A block regrouped by package can run on into lines that came earlier
    /// in the JSON: its item still ends at or after its first line.
    #[test]
    fn go_test_json_items_never_end_before_they_start() {
        let line = |pkg: &str, out: &str| format!(r#"{{"Time":"2026-11-03T00:00:00Z","Action":"output","Package":"{pkg}","Output":"{out}\n"}}"#);
        let text = [line("a", "start"), line("b", "x"), line("a", "filler"), line("a", "--- FAIL: TestX (0.00s)")].join("\n") + "\n";
        let items = items(Shape::Test(row("go test -json")), &text);
        assert!(!items.is_empty(), "{items:?}");
        for it in &items {
            assert!(it.first >= 1 && it.first <= it.last, "{it:?}");
        }
        let b = items.iter().find(|i| i.status == "failed-output" && i.name == "TestX").unwrap();
        assert_eq!(b.first, 4, "{b:?}");
    }

    #[test]
    fn jest_counts_from_its_totals_and_names_each_failure() {
        let want = [
            "jest: tests 2 passed 2 failed 1 skipped; files 1 ok 1 failed 0 without tests: failed",
            "FAILED math › adds sum.test.js:5 expect(received).toBe(expected) // Object.is equality; Expected: 4; Received: 5",
            "FAILED parses sum.js:5 SyntaxError: Expected ',' or '}' after property value in JSON at position 11 (line 1 column 12)",
        ];
        assert_eq!(head("npx jest", &out("js-test", "jest-fail.txt"), Some(1)), want);
        assert_eq!(head("npx jest --verbose", &out("js-test", "jest-fail-verbose.txt"), Some(1)), want, "--verbose: the same");
        assert_eq!(
            head("npx jest ok", &out("js-test", "jest-pass.txt"), Some(0)),
            ["jest: tests 1 passed 0 failed 0 skipped; files 1 ok 0 failed 0 without tests: passed"]
        );
        let text = out("js-test", "jest-fail.txt");
        let items = items(Shape::Test(row("jest")), &text);
        let b = items.iter().find(|i| i.status == "failed-output" && i.name == "parses").unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[b.first - 1].contains("● parses") && lines[b.last - 1].contains("sum.test.js:16:10"), "{b:?}");
    }

    #[test]
    fn vitest_reads_through_colour_codes() {
        assert_eq!(
            head("npx vitest run", &out("js-test", "vitest-fail.txt"), Some(1)),
            [
                "vitest: tests 2 passed 2 failed 1 skipped; files 1 ok 1 failed 0 without tests: failed",
                "FAILED sum.test.mjs > math > adds sum.test.mjs:6 AssertionError: expected 5 to be 4 // Object.is equality",
                "FAILED sum.test.mjs > parses ../jest/sum.js:5 SyntaxError: Expected ',' or '}' after property value in JSON at position 11 (line 1 column 12)",
            ]
        );
        let d = digest("h:1", "", shape("run", "", &out("js-test", "vitest-pass.txt")), &out("js-test", "vitest-pass.txt"), None, 600);
        assert!(
            d.contains(
                "vitest: tests 1 passed 0 failed 0 skipped; files 1 ok 0 failed 0 without tests: exit status unknown (inferred: passed)"
            ),
            "{d}"
        );
        assert!(!d.contains('\u{1b}'), "the tail is plain text: {d}");
    }

    #[test]
    fn pytest_counts_from_its_totals_and_reads_each_failure_report() {
        let failures = [
            "FAILED test_sub test_calc.py:11 assert 4 == 2; E        +  where 4 = sub(3, 1)",
            "FAILED test_div calc.py:10 ZeroDivisionError: division by zero",
            "FAILED TestText.test_strip test_more.py:6 AssertionError: assert 'a' == 'a '",
        ];
        let want: Vec<String> =
            std::iter::once("pytest: tests 4 passed 3 failed 1 skipped: failed".to_string()).chain(failures.iter().map(|f| f.to_string())).collect();
        assert_eq!(head("python -m pytest", &out("py-test", "fail.txt"), Some(1)), want);
        assert_eq!(head("pytest -v", &out("py-test", "fail-verbose.txt"), Some(1)), want, "-v: the same");
        assert_eq!(head("pytest", &out("py-test", "pass.txt"), Some(0)), ["pytest: tests 2 passed 0 failed 0 skipped: passed"]);
        // With -v the passes and skips are items too; a report runs to the
        // next report's header.
        let text = out("py-test", "fail-verbose.txt");
        let items = items(Shape::Test(row("pytest")), &text);
        let st = |s: &str| items.iter().filter(|i| i.status == s).count();
        assert_eq!((st("ok"), st("ignored"), st("failed")), (4, 1, 3));
        let lines: Vec<&str> = text.lines().collect();
        let b = items.iter().find(|i| i.status == "failed-output" && i.name == "test_div").unwrap();
        assert!(lines[b.first - 1].contains(" test_div ") && lines[b.last - 1].starts_with("calc.py:10:"), "{b:?}");
    }

    #[test]
    fn jvm_runners_count_from_their_totals_and_name_the_test_frame() {
        let mvn = |file: &str, exit: Option<i32>| head("mvn -B test", &out("mvn-test", file), exit);
        assert_eq!(
            mvn("fail.txt", Some(1)),
            [
                "mvn test: tests 2 passed 2 failed 1 skipped; classes 0 ok 1 failed 0 without tests: failed",
                "FAILED shapes.ShapesTest.kindIsMissing Square.java:33 java.lang.IllegalArgumentException: No enum constant shapes.Square.Kind.HUGE",
                "FAILED shapes.ShapesTest.areaIsWrong ShapesTest.java:21 org.opentest4j.AssertionFailedError: area of a 2 by 2 square ==> expected: <5.0> but was: <4.0>",
            ]
        );
        assert_eq!(mvn("pass.txt", Some(0)), ["mvn test: tests 3 passed 0 failed 0 skipped; classes 1 ok 0 failed 0 without tests: passed"]);
        let gradle = |file: &str, exit: Option<i32>| head("./gradlew test", &out("gradle-test", file), exit);
        assert_eq!(
            gradle("fail.txt", Some(1)),
            [
                "gradle test: tests 2 passed 2 failed 1 skipped; tasks 0 ok 1 failed 0 without tests: failed",
                "FAILED ShapesTest > kindIsMissing() ShapesTest.java:26 java.lang.IllegalArgumentException",
                "FAILED ShapesTest > areaIsWrong() ShapesTest.java:21 org.opentest4j.AssertionFailedError",
            ]
        );
        assert_eq!(
            gradle("pass.txt", Some(0)),
            ["gradle test: tests 0 passed 0 failed 0 skipped (Gradle lists failures only); tasks 1 ok 0 failed 0 without tests: passed"]
        );
    }
}
