//! The tests stay deterministic (Brandon, 2026-11-19: "all of our tests
//! should be completely deterministic"): no new real-clock wait or bound in
//! test code. A sleep used to wait for another thread or a file time, a
//! timed receive used as a deadline, or a wall-time bound on work passes or
//! fails with the load on the machine. Use an injected clock or sleeper
//! (virtual time), a channel, barrier or readiness signal, an explicit file
//! time (`File::set_modified`), or a count of the work done.
//!
//! Enforced forms: `sleep(` (a call named `sleep`, not `real_sleep(`),
//! `.elapsed()`, and `timeout` (`recv_timeout(`, `wait_timeout`,
//! `park_timeout(`). A bare `Instant::now()` is not counted: a virtual clock
//! starts from one.
//!
//! The uses there are now are listed, per file and kind with a reason, in
//! `tests/wall_clock_allow.txt`. A count must match exactly: one more is a
//! new use (fix it or list it with its reason), one fewer means a use was
//! removed and the list is lowered to match, so it only ever shrinks.
//!
//! Test code is every file under `tests/`, and in `src/` and `xtask/src/`
//! each item under a `#[cfg(...)]` naming `test` (not `not(test)`): an inline
//! `mod x { ... }` to its closing brace, and an out-of-line `mod x;` as the
//! whole file it names (`#[path]` honoured).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const KINDS: [&str; 3] = ["sleep", "elapsed", "timeout"];
const ALLOW: &str = "tests/wall_clock_allow.txt";

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// `line` with a `//` comment cut off, a `//` inside a string kept.
fn code_of(line: &str) -> &str {
    let b = line.as_bytes();
    let (mut in_str, mut i) = (false, 0);
    while i < b.len() {
        match b[i] {
            b'\\' if in_str => i += 1,
            b'"' => in_str = !in_str,
            b'/' if !in_str && b.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

/// The uses of each kind on one line of code.
fn uses(code: &str) -> [usize; 3] {
    let sleep = code.match_indices("sleep(").filter(|(i, _)| !code[..*i].ends_with(|c: char| c.is_alphanumeric() || c == '_')).count();
    let timeout = ["recv_timeout(", "wait_timeout", "park_timeout("].iter().map(|p| code.matches(p).count()).sum();
    [sleep, code.matches(".elapsed()").count(), timeout]
}

/// Whether `line` is a cfg attribute that holds only under test.
fn test_cfg(line: &str) -> bool {
    let t = line.trim();
    // `test` as a predicate, not inside a name (`feature = "test-seams"`).
    let token = t.match_indices("test").any(|(i, _)| {
        let (before, after) = (t[..i].trim_end().chars().last(), t[i + 4..].trim_start().chars().next());
        matches!(before, Some('(' | ',')) && matches!(after, Some(')' | ','))
    });
    t.starts_with("#[cfg(") && token && !t.contains("not(test)")
}

/// The module folder of the file at `rel`: where its `mod x;` files live.
fn mod_dir(rel: &str) -> String {
    let (dir, file) = rel.rsplit_once('/').unwrap_or(("", rel));
    match file {
        "mod.rs" | "lib.rs" | "main.rs" => dir.to_string(),
        f => format!("{dir}/{}", f.trim_end_matches(".rs")),
    }
}

/// The test lines of `text` at `rel` (a path with `/`), and the files its
/// out-of-line test modules name (scanned whole).
fn test_lines<'a>(rel: &str, text: &'a str) -> (Vec<&'a str>, Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    if rel.starts_with("tests/") {
        return (lines, Vec::new());
    }
    let (mut out, mut files, mut i) = (Vec::new(), Vec::new(), 0);
    while i < lines.len() {
        if !test_cfg(lines[i]) {
            i += 1;
            continue;
        }
        // Past the other attributes to the item, noting a `#[path]`.
        let mut j = i + 1;
        let mut path = None;
        while j < lines.len() && lines[j].trim_start().starts_with("#[") {
            if let Some(p) = lines[j].trim().strip_prefix("#[path = \"").and_then(|p| p.strip_suffix("\"]")) {
                path = Some(p.to_string());
            }
            j += 1;
        }
        let Some(item) = lines.get(j).map(|l| code_of(l).trim()) else { break };
        let is_mod = item.split_whitespace().take_while(|w| *w != "{").any(|w| w == "mod");
        if is_mod && item.ends_with(';') {
            let name = item.trim_end_matches(';').split_whitespace().last().unwrap_or("");
            let dir = mod_dir(rel);
            files.push(match path {
                Some(p) => format!("{}/{p}", rel.rsplit_once('/').map_or("", |(d, _)| d)),
                None => format!("{dir}/{name}.rs"),
            });
            i = j + 1;
            continue;
        }
        // An inline item: to the brace that closes its first one (an item
        // with no brace, a `use` say, is one line).
        let mut depth = 0i64;
        let mut opened = false;
        let mut k = j;
        while k < lines.len() {
            for c in code_of(lines[k]).chars() {
                match c {
                    '{' => (depth, opened) = (depth + 1, true),
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if (opened && depth <= 0) || (!opened && code_of(lines[k]).trim_end().ends_with(';')) {
                break;
            }
            k += 1;
        }
        let end = k.min(lines.len() - 1);
        out.extend_from_slice(&lines[i..=end]);
        i = end + 1;
    }
    (out, files)
}

/// Counts by (path, kind), comments left out.
fn found(root: &Path) -> BTreeMap<(String, String), usize> {
    let mut files = Vec::new();
    for d in ["tests", "src", "xtask/src"] {
        rs_files(&root.join(d), &mut files);
    }
    let rels: Vec<String> = files.iter().map(|f| f.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/")).collect();
    let texts: BTreeMap<&str, String> = rels.iter().map(|r| (r.as_str(), std::fs::read_to_string(root.join(r)).unwrap())).collect();
    // Files that are test modules of another file, scanned whole.
    let mut whole: BTreeSet<String> = BTreeSet::new();
    let mut parts: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (rel, text) in &texts {
        let (lines, mods) = test_lines(rel, text);
        whole.extend(mods);
        parts.insert(rel, lines);
    }
    let mut out = BTreeMap::new();
    for (rel, text) in &texts {
        if *rel == "tests/wall_clock_lint.rs" {
            continue;
        }
        let lines: Vec<&str> = if whole.contains(*rel) { text.lines().collect() } else { parts[rel].clone() };
        for line in lines {
            for (kind, n) in KINDS.iter().zip(uses(code_of(line))) {
                if n > 0 {
                    *out.entry((rel.to_string(), kind.to_string())).or_insert(0) += n;
                }
            }
        }
    }
    out
}

/// The allow-list: `COUNT KIND PATH -- REASON` per line, `#` comments.
fn allowed(text: &str) -> Result<BTreeMap<(String, String), usize>, String> {
    let mut out = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (head, reason) = line.split_once(" -- ").ok_or(format!("{ALLOW}:{}: no ` -- REASON`", i + 1))?;
        if reason.trim().is_empty() {
            return Err(format!("{ALLOW}:{}: an empty reason", i + 1));
        }
        let f: Vec<&str> = head.split_whitespace().collect();
        let [n, kind, path] = f[..] else { return Err(format!("{ALLOW}:{}: not `COUNT KIND PATH`", i + 1)) };
        let n: usize = n.parse().map_err(|_| format!("{ALLOW}:{}: count {n:?}", i + 1))?;
        if !KINDS.contains(&kind) {
            return Err(format!("{ALLOW}:{}: kind {kind:?} is not one of {KINDS:?}", i + 1));
        }
        if out.insert((path.to_string(), kind.to_string()), n).is_some() {
            return Err(format!("{ALLOW}:{}: {path} {kind} listed twice", i + 1));
        }
    }
    Ok(out)
}

#[test]
fn no_new_sleep_or_wall_time_in_tests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let have = found(root);
    let allow = allowed(&std::fs::read_to_string(root.join(ALLOW)).unwrap()).unwrap();
    let mut errors = Vec::new();
    for ((path, kind), n) in &have {
        match allow.get(&(path.clone(), kind.clone())) {
            None => errors.push(format!(
                "{path}: {n} new {kind} use(s) in test code; use virtual time, a signal or a count, or list it in {ALLOW} with its reason"
            )),
            Some(a) if n > a => {
                errors.push(format!("{path}: {n} {kind} uses, {a} allowed: a new one; use virtual time, a signal or a count"))
            }
            Some(a) if n < a => errors.push(format!("{path}: {n} {kind} uses, {a} allowed: lower the count in {ALLOW} to {n}")),
            Some(_) => {}
        }
    }
    for ((path, kind), a) in &allow {
        if !have.contains_key(&(path.clone(), kind.clone())) {
            errors.push(format!("{path}: no {kind} use left, {a} allowed: remove the line from {ALLOW}"));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn the_scanner_sees_test_modules_only() {
    let src = "fn f() { std::thread::sleep(d); }\n#[cfg(test)]\nmod tests {\n    fn g() { std::thread::sleep(d); t.elapsed(); }\n    // a comment: sleep(\n}\n";
    let (lines, mods) = test_lines("src/x.rs", src);
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert!(mods.is_empty() && !lines.iter().any(|l| l.starts_with("fn f")));
    assert_eq!(test_lines("tests/x.rs", src).0.len(), 6);
    // A test module in the middle of a file: the code after it is not test code.
    let mid = "#[cfg(all(test, unix))]\n#[allow(dead_code)]\nmod t {\n    fn g() { sleep(d); }\n}\nfn after() { sleep(d); }\n";
    let (lines, _) = test_lines("src/x.rs", mid);
    assert_eq!(lines.len(), 5, "{lines:?}");
    // Out-of-line test modules: the files they name, `#[path]` honoured.
    let out = "#[cfg(test)]\nmod fix_tests;\n#[cfg(test)]\n#[path = \"t/p.rs\"]\npub(crate) mod p;\n#[cfg(not(test))]\nmod real;\n";
    assert_eq!(test_lines("src/a/mod.rs", out).1, ["src/a/fix_tests.rs", "src/a/t/p.rs"]);
    assert_eq!(test_lines("src/lex.rs", "#[cfg(test)]\npub(crate) mod props;\n").1, ["src/lex/props.rs"]);
    // The kinds: a call named sleep only, comments cut, a `//` in a string kept.
    assert_eq!(uses("real_sleep(d); thread::sleep(d); t.elapsed(); rx.recv_timeout(d)"), [1, 1, 1]);
    assert_eq!(uses(code_of("let u = \"http://x\"; sleep(d); // sleep(")), [1, 0, 0]);
    assert!(allowed("2 sleep src/x.rs").is_err(), "a line with no reason");
    assert!(allowed("2 nap src/x.rs -- r").is_err(), "an unknown kind");
    assert_eq!(allowed("# c\n2 sleep src/x.rs -- a reason\n").unwrap().len(), 1);
}
