//! Hostile queries: FTS5 syntax and SQL metacharacters in every string
//! argument, pathological regexes and globs at the tool surface, empty and
//! whitespace queries, budgets and limits at their edges.

use std::time::{Duration, Instant};

use crate::common::*;
use crate::gen::{each, HOSTILE};
use atlas::json;
use atlas::json::Value;

const FTS: &[&str] = &[
    "\"", "\"\"", "*", "^", "-", "+", ":", "(", ")", "{", "}", "NEAR", "NEAR(", "NEAR/2", "AND", "OR", "NOT", "body:", "heading:", "{body}:", "-body:",
    "a*", "^a", "'", ";", "--", "/*", "%", "_", "\\", "\0", ",", ".", "@", "#", "$", "&", "|", "~", "`", "[", "]", " ", "\t", "\n", "cache", "loader",
    "\u{130}", "\u{e9}", "\u{1f980}", "x\u{301}",
];

fn text(v: &Value) -> String {
    v.get("text").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| v.to_string())
}

/// A sections query (not raw) is always turned into valid FTS5: whatever
/// the client types, the answer is results or none, never a syntax error.
/// The vault `fts` mode takes FTS5 syntax as written: an error there is an
/// invalid-argument answer naming FTS5, never a panic.
#[test]
fn plain_queries_are_never_fts_syntax_errors() {
    let fx = Fx::new("adv-fts");
    let mut s = fx.server();
    each(20, 600, |r| r.text_from(FTS, 8), |q| {
        if let Err(e) = s.call("search", &json!({"query": q, "mode": "sections"})) {
            let e = e.to_string();
            assert!(!e.contains("fts") && !e.contains("syntax"), "sections {q:?}: {e}");
        }
        if let Err(e) = s.call("search", &json!({"query": q, "mode": "fts"})) {
            assert!(e.to_string().contains("fts") || e.to_string().contains("query must be"), "fts {q:?}: {e}");
        }
    });
}

/// Raw FTS5 is the caller's syntax: an error names it, nothing panics, and
/// a column filter cannot reach another table.
#[test]
fn raw_fts_errors_are_clean() {
    let fx = Fx::new("adv-fts-raw");
    let mut s = fx.server();
    for q in [
        "\"", "NEAR(", "a NEAR/x b", "body:", "nosuchcolumn:x", "{body path}:x", "*", "^", "a OR", "(a", "a)", "-a", "x\"; DROP TABLE notes; --",
        "') UNION SELECT * FROM writes --", "a AND NOT", "NEAR(a b, -1)", "NEAR(a b, 99999999999)",
    ] {
        for mode in ["fts", "sections"] {
            let _ = s.call("search", &json!({"query": q, "mode": mode, "raw": true}));
        }
    }
    // The database is intact afterwards.
    let r = call(&mut s, "search", json!({"query": "loader", "mode": "fts"}));
    assert!(r.to_string().contains("PI-001"), "{r}");
}

/// SQL metacharacters in filters are matched literally: they never widen a
/// filter or break a statement.
#[test]
fn sql_metacharacters_in_filters_are_literal() {
    let fx = Fx::new("adv-sql");
    let mut s = fx.server();
    for v in ["perf' OR '1'='1", "perf\"", "%", "_", "perf;--", "perf\0"] {
        let e = s.call("list_notes", &json!({"vault": v}));
        assert!(e.is_err(), "vault {v:?} was served");
        let r = s.call("search", &json!({"query": "loader", "vault": v}));
        if let Ok(r) = r {
            assert!(!r.to_string().contains("PI-001"), "vault filter {v:?} matched perf: {r}");
        }
    }
    for k in ["status' OR 1=1 --", "%", "_", "id\"", "tags"] {
        let _ = s.call("find_by_frontmatter", &json!({"key": k, "value": "%"}));
    }
    let r = call(&mut s, "find_by_frontmatter", json!({"key": "status", "value": "%"}));
    assert_eq!(r["notes"].as_array().map(Vec::len).unwrap_or(0), 0, "a % value matched like a wildcard: {r}");
    for author in ["%", "_", "' OR 1=1 --"] {
        let r = call(&mut s, "write_log", json!({"client": author}));
        assert_eq!(r["writes"].as_array().map(Vec::len).unwrap_or(0), 0, "client {author:?}: {r}");
    }
}

/// The `folder` filter of a sections search is a path prefix: `%` and `_`
/// are not LIKE wildcards.
#[test]
fn a_sections_folder_filter_is_not_a_like_pattern() {
    let fx = Fx::new("adv-like");
    let mut s = fx.server();
    let all = call(&mut s, "search", json!({"query": "throttle", "mode": "sections", "format": "json"}));
    assert!(all.to_string().contains("Throttle"), "{all}");
    for f in ["%", "_deas", "I%", "%/%"] {
        let r = call(&mut s, "search", json!({"query": "throttle", "mode": "sections", "folder": f, "format": "json"}));
        assert!(!r.to_string().contains("PI-002"), "folder {f:?} matched Ideas/: {r}");
    }
}

#[test]
fn empty_and_whitespace_queries() {
    let fx = Fx::new("adv-empty");
    let mut s = fx.server();
    for q in ["", " ", "\t\n", "\u{3000}", "\u{200b}", "\"\"", "\" \"", "*", "-", "\0"] {
        for mode in ["fts", "substring", "regex", "sections"] {
            let _ = s.call("search", &json!({"query": q, "mode": mode}));
        }
        let _ = s.call("lookup", &json!({"id": q}));
        let _ = s.call("ids", &json!({"prefix": q}));
        let _ = s.call("grep", &json!({"pattern": q}));
        let _ = s.call("section", &json!({"heading": q}));
    }
}

#[test]
fn limits_at_their_edges() {
    let fx = Fx::new("adv-limits");
    let mut s = fx.server();
    for limit in [json!(0), json!(1), json!(-1), json!(1.5), json!(u64::MAX), json!(i64::MAX), json!(1e300), json!("10")] {
        for (tool, args) in [
            ("list_notes", json!({"vault": "perf"})),
            ("search", json!({"query": "a"})),
            ("lookup", json!({"id": "PI-001"})),
            ("ids", json!({})),
            ("misses", json!({})),
            ("find_by_frontmatter", json!({"key": "status"})),
            ("write_log", json!({})),
        ] {
            let mut a = args;
            a.as_object_mut().unwrap().insert("limit".into(), limit.clone());
            let _ = s.call(tool, &a);
        }
    }
    let r = s.call("list_notes", &json!({"vault": "perf", "limit": 0}));
    if let Ok(r) = r {
        assert_eq!(r["notes"].as_array().map(Vec::len).unwrap_or(0), 0, "limit 0 returned notes: {r}");
    }
    let e = call_err(&mut s, "grep", json!({"pattern": "x".repeat(1025)}));
    assert!(e.contains("1024"), "{e}");
    let e = call_err(&mut s, "grep", json!({"pattern": "x", "context": 11}));
    assert!(e.contains("context"), "{e}");
}

/// Pathological patterns at the tool surface finish quickly.
#[test]
fn pathological_patterns_through_the_tools() {
    let fx = Fx::new("adv-patho");
    write(&fx.perf().join("Long.md"), &("a".repeat(200) + "\n").repeat(500));
    write(&fx.perf().join(format!("{}.md", "a".repeat(200))), "long name\n");
    let mut s = fx.server();
    let t = Instant::now();
    for p in ["(a*)*b", "(a+)+b", "(a|aa)+$", "(.*a){12}", "^(\\w+\\s?)*$"] {
        let _ = s.call("search", &json!({"query": p, "mode": "regex"}));
        let _ = s.call("ids", &json!({"pattern": p}));
        let _ = s.call("grep", &json!({"pattern": p}));
    }
    // Within the tool's own cap (8 stars, 128 bytes), and past it.
    for g in ["*a*a*a*a*a*a*a*a*b", "**a**a**a**a*b", "?*?*?*?*?*?*?*?*b", "*a*a*a*a*a*a*a*a*a*a*a*a*b"] {
        if let Ok(r) = s.call("list_notes", &json!({"vault": "perf", "glob": g})) {
            assert_eq!(r["notes"].as_array().map(Vec::len).unwrap_or(0), 0, "{g}");
        }
    }
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
}

/// A regex search whose backtracking state would pass the budget on some
/// line is refused with an error naming the note, the line and what to
/// narrow; a literal search of the same line is not.
#[test]
fn regex_search_over_the_budget_is_refused_with_a_reason() {
    let fx = Fx::new("adv-regex-budget");
    write(&fx.perf().join("Wide.md"), &format!("short line\n{}\n", "a".repeat(600 << 10)));
    let mut s = fx.server();
    let e = s.call("search", &json!({"query": "(?:[a-z]\\d|\\d[a-z]){600}", "mode": "regex"})).unwrap_err().to_string();
    assert!(e.contains("refused") && e.contains("Wide.md line 2") && e.contains("narrow the pattern"), "{e}");
    let r = s.call("search", &json!({"query": "aaaa", "mode": "substring"})).unwrap();
    assert!(text(&r).contains("Wide.md"), "{r}");
    // An ordinary pattern searches the same wide line.
    let r = s.call("search", &json!({"query": "short (?:line|row)|a{50}$", "mode": "regex"})).unwrap();
    let lines: Vec<i64> = r["results"].as_array().unwrap().iter().filter_map(|h| h["line"].as_i64()).collect();
    assert_eq!(lines, vec![1, 2], "{r}");
}

/// Hostile strings as search input across every mode never panic and
/// never return a note that does not hold the query.
#[test]
fn hostile_search_strings() {
    let fx = Fx::new("adv-hostile-search");
    let mut s = fx.server();
    each(21, 300, |r| r.text_from(HOSTILE, 5), |q| {
        for mode in ["substring", "regex", "fts", "sections"] {
            let _ = s.call("search", &json!({"query": q, "mode": mode, "case_sensitive": true}));
        }
        let _ = s.call("lookup", &json!({"id": q}));
        let _ = s.call("section", &json!({"heading": q}));
        let _ = s.call("section", &json!({"id": q}));
    });
    let r = call(&mut s, "search", json!({"query": "zz-not-anywhere-zz", "mode": "substring"}));
    assert!(!text(&r).contains(".md"), "{r}");
}

/// The `section` heading lookup matches a nested heading by its last
/// part: `%` and `_` in the heading are literal, not LIKE wildcards.
#[test]
fn a_section_heading_is_not_a_like_pattern() {
    let fx = Fx::new("adv-like-heading");
    let mut s = fx.server();
    for h in ["%", "_istory", "H%", "%y"] {
        if let Ok(r) = s.call("section", &json!({"heading": h})) {
            assert!(!r.to_string().contains("measured at 12 us"), "heading {h:?} matched History: {r}");
        }
    }
    let r = call(&mut s, "section", json!({"heading": "History"}));
    assert!(r.to_string().contains("measured at 12 us"), "{r}");
}
