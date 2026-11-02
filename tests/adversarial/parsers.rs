//! Property tests and fuzz loops for every hand-written parser that reads
//! text a client, a note or a source file controls: JSON, the regex engine,
//! globs, Markdown and frontmatter, links, the chunker, the code and WGSL
//! scanners, discussions, digests, shell-command parsing, protobuf and SCIP.
//!
//! Each `fuzz_*` runs a fixed number of cases; `deep_fuzz_*` (ignored) runs
//! [`DEEP`] times as many.

use std::time::{Duration, Instant};

use crate::gen::{boundary, each, Rng, DEEP, HOSTILE};
use atlas::json::{self, Map, Value};
use atlas::regex::{escape, Regex, RegexBuilder};

// ------------------------------------------------------------------ JSON

fn gen_value(r: &mut Rng, depth: usize) -> Value {
    match r.below(if depth > 4 { 5 } else { 7 }) {
        0 => Value::Null,
        1 => Value::Bool(r.chance(1, 2)),
        2 => match r.below(4) {
            0 => atlas::json!(r.next()),
            1 => atlas::json!(-((r.next() >> 2) as i64)),
            2 => atlas::json!(((r.next() >> 11) as f64) / 1024.0 - 1e9),
            _ => atlas::json!(r.below(10)),
        },
        3 | 4 => Value::String(r.text_from(HOSTILE, 8)),
        5 => Value::Array((0..r.below(5)).map(|_| gen_value(r, depth + 1)).collect()),
        _ => {
            let mut m = Map::new();
            for _ in 0..r.below(5) {
                m.insert(r.text_from(HOSTILE, 3), gen_value(r, depth + 1));
            }
            Value::Object(m)
        }
    }
}

fn json_round_trips(n: usize) {
    each(1, n, |r| gen_value(r, 0), |v| {
        let compact = v.to_string();
        let back = json::parse(&compact).unwrap_or_else(|e| panic!("own output does not parse: {e}: {compact}"));
        assert_eq!(&back, v, "compact round trip");
        let pretty = json::to_string_pretty(v);
        assert_eq!(&json::parse(&pretty).unwrap(), v, "pretty round trip");
        // Writing is a fixed point.
        assert_eq!(back.to_string(), compact);
    });
}

fn json_mutations_never_panic(n: usize) {
    each(2, n, |r| {
        let mut b = gen_value(r, 0).to_string().into_bytes();
        r.mutate(&mut b);
        b
    }, |b| {
        if let Ok(v) = json::parse_bytes(b) {
            // Whatever is accepted writes back to text that parses the same.
            assert_eq!(json::parse(&v.to_string()).unwrap(), v);
        }
    });
}

#[test]
fn fuzz_json() {
    json_round_trips(4000);
    json_mutations_never_panic(6000);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_json() {
    json_round_trips(4000 * DEEP);
    json_mutations_never_panic(6000 * DEEP);
}

#[test]
fn json_deep_nesting_is_an_error_not_a_stack_overflow() {
    for open in ["[", "{\"a\":", "[{\"k\":[", "[[[[{\"\":"] {
        let text = open.repeat(200_000);
        let e = json::parse(&text).expect_err("deep nesting");
        assert!(e.msg.contains("nesting too deep"), "{open}: {e}");
    }
    // Deep but legal nesting is accepted (128 levels).
    let ok = "[".repeat(json::MAX_DEPTH) + &"]".repeat(json::MAX_DEPTH);
    assert!(json::parse(&ok).is_ok());
}

#[test]
fn json_huge_strings_and_numbers() {
    let s = "x".repeat(8 << 20);
    let v = json::parse(&format!("\"{s}\"")).unwrap();
    assert_eq!(v.as_str().map(str::len), Some(8 << 20));
    let digits = "9".repeat(100_000);
    assert!(json::parse(&digits).is_err(), "a 100k-digit integer is past f64");
    assert!(json::parse(&format!("0.{digits}")).is_ok());
    assert_eq!(json::parse("1e-400").unwrap().as_f64(), Some(0.0));
    assert_eq!(json::parse("-0").unwrap().as_f64(), Some(0.0));
    assert_eq!(json::parse("18446744073709551615").unwrap().as_u64(), Some(u64::MAX));
    assert_eq!(json::parse("-9223372036854775808").unwrap().as_i64(), Some(i64::MIN));
    assert!(json::parse("-9223372036854775809").unwrap().as_i64().is_none());
    for bad in ["+1", ".5", "1e", "1e+", "0x10", "Infinity", "-Infinity", "1_000", "١"] {
        assert!(json::parse(bad).is_err(), "{bad} accepted");
    }
}

#[test]
fn json_escapes_surrogates_and_invalid_utf8() {
    assert_eq!(json::parse(r#""😀""#).unwrap().as_str(), Some("\u{1F600}"));
    assert_eq!(json::parse(r#""􏿿""#).unwrap().as_str(), Some("\u{10FFFF}"));
    assert_eq!(json::parse(r#""\u0000""#).unwrap().as_str(), Some("\0"));
    for bad in [r#""\uDE00\uD83D""#, r#""\uD83D""#, r#""\uD83Dx""#, r#""\uD83D\n""#, r#""\u+123""#, r#""\u-123""#, r#""\u 123""#, r#""\uZZZZ""#, r#""\U0041""#] {
        assert!(json::parse(bad).is_err(), "{bad} accepted");
    }
    for bytes in [&b"\"\xc0\x80\""[..], b"\"\xed\xa0\x80\"", b"\"\xf4\x90\x80\x80\"", b"\"\xe2\x82\"", b"\xff", b"[\"a\",\xfe]"] {
        assert!(json::parse_bytes(bytes).is_err(), "{bytes:?} accepted");
    }
    // Every control character survives the writer and the parser.
    let all: String = (0u8..0x20).map(char::from).collect();
    let v = Value::String(all.clone());
    assert_eq!(json::parse(&v.to_string()).unwrap().as_str(), Some(all.as_str()));
    // Unicode white space is not JSON white space.
    assert!(json::parse("\u{a0}1").is_err());
    assert!(json::parse("1\u{2028}").is_err());
}

#[test]
fn json_duplicate_keys_keep_the_last_value_in_the_first_place() {
    let v = json::parse(r#"{"path":"a.md","x":1,"path":"../b.md"}"#).unwrap();
    let m = v.as_object().unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!(m.get("path").and_then(Value::as_str), Some("../b.md"));
    assert_eq!(m.keys().next().map(String::as_str), Some("path"));
}

/// An object with many keys parses in time linear in its size: a 4 MiB
/// message is the protocol's cap, and it holds about 300k short keys.
#[test]
fn json_many_keys_parse_in_linear_time() {
    let n = 300_000;
    let mut s = String::from("{");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("\"k{i}\":0"));
    }
    s.push('}');
    assert!(s.len() < (4 << 20));
    let t = Instant::now();
    let v = json::parse(&s).unwrap();
    let took = t.elapsed();
    assert_eq!(v.as_object().unwrap().len(), n);
    assert!(took < Duration::from_secs(3), "{n} keys took {took:?}");
}

// ------------------------------------------------------------------ regex

const RE_TOKENS: &[&str] = &[
    "a", "b", "ab", ".", "\\.", "\\d", "\\w", "\\s", "\\D", "\\W", "\\S", "\\b", "\\B", "\\p{L}", "\\P{N}", "[a-c]", "[^x]", "[\\d_]", "[", "]",
    "(", ")", "(?:", "(?P<n>", "(?<m>", "|", "*", "+", "?", "*?", "+?", "??", "{2}", "{1,3}", "{0,}", "{2,1}", "{1000}", "{1001}", "^", "$",
    "(?i)", "(?m)", "(?s)", "\\", "\\x41", "\\1", "(?=a)", "\u{e9}", "\u{130}", "\u{1f980}", "-", "TK-", "\\n", "\\t",
];

fn regex_properties(n: usize) {
    each(3, n, |r| (r.text_from(RE_TOKENS, 10), r.text_from(HOSTILE, 30), r.chance(1, 3)), |(p, text, fold)| {
        let Ok(re) = RegexBuilder::new(p).case_insensitive(*fold).build() else { return };
        let m = re.find(text);
        assert_eq!(re.is_match(text), m.is_some());
        if let Some(m) = m {
            assert!(m.start() <= m.end() && boundary(text, m.start()) && boundary(text, m.end()));
            assert_eq!(m.as_str(), &text[m.start()..m.end()]);
            let c = re.captures(text).expect("captures where find matched");
            let whole = c.get(0).unwrap();
            assert_eq!((whole.start(), whole.end()), (m.start(), m.end()));
            for i in 0..c.len() {
                if let Some(g) = c.get(i) {
                    assert!(boundary(text, g.start()) && boundary(text, g.end()) && g.start() <= g.end());
                }
            }
        }
        let mut last = 0;
        for (k, m) in re.find_iter(text).enumerate() {
            assert!(m.start() >= last, "find_iter went backwards");
            assert!(boundary(text, m.start()) && boundary(text, m.end()));
            last = m.end();
            assert!(k <= text.len() + 1, "find_iter does not terminate");
        }
        for from in [0, text.len() / 2, text.len()] {
            if text.is_char_boundary(from) {
                if let Some(m) = re.find_at(text, from) {
                    assert!(m.start() >= from);
                }
            }
        }
    });
}

fn regex_escape_is_literal(n: usize) {
    each(4, n, |r| (r.text_from(HOSTILE, 6), r.text_from(HOSTILE, 6), r.text_from(HOSTILE, 6)), |(lit, before, after)| {
        if lit.is_empty() {
            return;
        }
        let re = Regex::new(&escape(lit)).unwrap_or_else(|e| panic!("escape({lit:?}) does not compile: {e}"));
        let hay = format!("{before}{lit}{after}");
        let want = hay.find(lit.as_str());
        assert_eq!(re.find(&hay).map(|m| m.start()), want, "escaped literal finds the first occurrence");
        // Case-insensitively an ASCII literal matches its upper-case copy.
        if lit.is_ascii() {
            let re = RegexBuilder::new(&escape(lit)).case_insensitive(true).build().unwrap();
            assert!(re.is_match(&lit.to_ascii_uppercase()));
        }
    });
}

#[test]
fn fuzz_regex() {
    regex_properties(6000);
    regex_escape_is_literal(3000);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_regex() {
    regex_properties(6000 * DEEP);
    regex_escape_is_literal(3000 * DEEP);
}

/// The textbook catastrophic patterns run in time linear in the text: the
/// engine records every (instruction, position) pair it tries.
#[test]
fn regex_pathological_patterns_are_bounded() {
    let cases: &[(&str, String)] = &[
        ("(a*)*b", "a".repeat(20_000)),
        ("(a+)+b", "a".repeat(20_000)),
        ("(a|a)*b", "a".repeat(20_000)),
        ("(a|aa)+$", "a".repeat(20_000) + "b"),
        ("(x+x+)+y", "x".repeat(20_000)),
        ("(.*a){20}", "a".repeat(5_000)),
        ("^(\\w+\\s?)*$", "word ".repeat(4_000) + "!"),
        ("(?i)(\u{130}|i)+x", "\u{130}i".repeat(5_000)),
        ("(?:a?){50}a{50}", "a".repeat(50)),
    ];
    for (p, text) in cases {
        let re = Regex::new(p).unwrap();
        let t = Instant::now();
        let _ = re.find(text);
        let _ = re.find_iter(text).count();
        assert!(t.elapsed() < Duration::from_secs(5), "{p} took {:?}", t.elapsed());
    }
}

#[test]
fn regex_refuses_oversized_and_unsupported_patterns() {
    for p in ["a{1001}", "(a{1000}){1000}", "(?:(?:(?:a{100}){100}){100})", "a{2,1}", "(?<=a)b", "(a)\\1", "\\x41", "(?i)a(?m)b", "[z-a]", "(", ")", "[", "a**", "*", "+a", "{1}", "\\p{Greek}", "\\"] {
        assert!(Regex::new(p).is_err(), "{p:?} compiled");
    }
    // Deep group nesting is refused or compiled, never a stack overflow.
    let deep = "(".repeat(50_000) + "a" + &")".repeat(50_000);
    let _ = Regex::new(&deep);
    let alt = "a|".repeat(50_000) + "a";
    let _ = Regex::new(&alt);
}

/// The backtracker's visited set is a bitset over every (instruction,
/// position) pair, so its size is program length times text length. A
/// search over the budget is refused (and says what to narrow), and a set
/// grown by a large search is freed after it.
#[test]
fn regex_memory_is_bounded_on_a_long_line() {
    let p = "(?:[a-z]\\d|\\d[a-z]){600}";
    let re = Regex::new(p).unwrap();
    let line = "a".repeat(1 << 20);
    let t = Instant::now();
    assert!(!re.is_match(&line));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let e = re.check_text(line.len()).unwrap_err().to_string();
    assert!(e.contains("instructions") && e.contains("narrow the pattern"), "{e}");
    assert!(atlas::regex::kept_bytes() <= 1 << 20, "{} bytes kept", atlas::regex::kept_bytes());
}

/// Under the budget a long line is searched in full, and the set it grew
/// is freed afterwards; a pattern that cannot backtrack needs no set, so
/// even a long literal searches a long line.
#[test]
fn regex_long_lines_under_the_budget_still_match() {
    let mut line = "ab1".repeat(300_000);
    line.push_str("zz9q");
    let re = Regex::new("(?:z|y)+\\dq").unwrap();
    assert!(re.check_text(line.len()).is_ok());
    assert!(re.visited_bits(line.len()) > 4 << 20, "the set is large enough to be freed");
    assert_eq!(re.find(&line).map(|m| m.as_str()), Some("zz9q"));
    assert!(atlas::regex::kept_bytes() <= 1 << 20, "{} bytes kept", atlas::regex::kept_bytes());
    let lit = "word ".repeat(100) + "end";
    let hay = "x".repeat(1 << 20) + &lit;
    let re = atlas::regex::RegexBuilder::new(&atlas::regex::escape(&lit)).case_insensitive(true).build().unwrap();
    assert_eq!(re.visited_bits(hay.len()), 0, "a literal never backtracks");
    assert!(re.check_text(hay.len()).is_ok());
    assert_eq!(re.find(&hay).map(|m| m.start()), Some(1 << 20));
    // Exactly at the budget is allowed; one byte more is not.
    let re = Regex::new("a|b").unwrap();
    let per = re.visited_bits(0);
    let fits = atlas::regex::MAX_VISITED_BITS / per - 1;
    assert!(re.check_text(fits).is_ok());
    assert!(re.check_text(fits + 1).is_err());
}

// ------------------------------------------------------------------ glob

/// The glob semantics, as a plain backtracking reference.
fn glob_ref(pattern: &str, path: &str) -> bool {
    fn rec(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') if p.get(1) == Some(&'*') => {
                let rest = &p[2..];
                if rest.first() == Some(&'/') && rec(&rest[1..], t) {
                    return true;
                }
                (0..=t.len()).any(|i| rec(rest, &t[i..]))
            }
            Some('*') => {
                for i in 0..=t.len() {
                    if rec(&p[1..], &t[i..]) {
                        return true;
                    }
                    if i < t.len() && t[i] == '/' {
                        break;
                    }
                }
                false
            }
            Some('?') => !t.is_empty() && t[0] != '/' && rec(&p[1..], &t[1..]),
            Some(c) => !t.is_empty() && t[0] == *c && rec(&p[1..], &t[1..]),
        }
    }
    let target = if pattern.contains('/') { path } else { path.rsplit('/').next().unwrap_or(path) };
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = target.to_lowercase().chars().collect();
    rec(&p, &t)
}

fn glob_matches_reference(n: usize) {
    const G: &[&str] = &["*", "**", "?", "/", "a", "b", "A", ".md", "**/", "\u{130}", "\u{e9}", "[", "x"];
    each(5, n, |r| (r.text_from(G, 6), r.text_from(G, 8)), |(p, t)| {
        assert_eq!(atlas::glob::matches(p, t), glob_ref(p, t), "glob {p:?} on {t:?}");
    });
}

#[test]
fn fuzz_glob() {
    glob_matches_reference(20_000);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_glob() {
    glob_matches_reference(20_000 * DEEP);
}

/// Many stars against a long name that never matches: a backtracking glob
/// is exponential in the number of stars.
#[test]
fn glob_pathological_patterns_are_bounded() {
    let name = "a".repeat(200);
    let path = "a/".repeat(100) + &name;
    for (p, t) in [
        ("*a*a*a*a*a*a*a*a*a*a*a*a*b".to_string(), name.as_str()),
        ("**a**a**a**a**a**a**a**a**b".to_string(), path.as_str()),
        ("**/".repeat(12) + "b", path.as_str()),
        ("?*".repeat(30) + "b", name.as_str()),
    ] {
        let t0 = Instant::now();
        assert!(!atlas::glob::matches(&p, t));
        assert!(t0.elapsed() < Duration::from_secs(1), "{p} took {:?}", t0.elapsed());
    }
}

// ------------------------------------------------------------------ Markdown

fn markdown_never_panics(n: usize) {
    use atlas::markdown as md;
    each(6, n, |r| (r.text_from(HOSTILE, 60), r.text_from(HOSTILE, 3), r.text_from(HOSTILE, 4)), |(text, name, content)| {
        let spans = md::line_spans(text);
        let mut at = 0;
        for &(s, e, next) in &spans {
            assert!(s == at && s <= e && e <= next && boundary(text, e) && boundary(text, next), "spans {spans:?}");
            at = next;
        }
        assert_eq!(md::code_lines(text, &spans).len(), spans.len());
        let _ = md::frontmatter_span(text);
        assert!(boundary(text, md::body_start(text)));
        let _ = md::body(text);
        let fm = md::parse_frontmatter(text);
        let _ = md::parse_frontmatter_nested(text);
        let _ = md::extract_tags(text, &fm);
        for l in md::extract_links(text) {
            assert!(l.line >= 1 && l.line <= spans.len().max(1), "link line {} of {}", l.line, spans.len());
        }
        for h in md::headings(text) {
            assert!((1..=6).contains(&h.level) && boundary(text, h.start) && boundary(text, h.line_end) && h.start < h.line_end.max(h.start + 1));
        }
        for level in [None, Some(1), Some(6), Some(0), Some(usize::MAX)] {
            if let Ok(Some(s)) = md::find_section(text, name, level) {
                let _ = s;
            }
            let _ = md::replace_section(text, name, level, content);
        }
        let _ = md::append_to_section(text, name, content);
        let _ = md::replace_preamble(text, content);
        let _ = md::to_eol(content, md::eol(text));
        let mut up = Map::new();
        up.insert("status".into(), Value::String(content.clone()));
        if let Ok(out) = md::set_frontmatter(text, &up) {
            if !content.contains(['\n', '\r', '\0']) {
                let back = md::parse_frontmatter(&out);
                assert!(back.get("status").is_some(), "the key written is read back: {out:?}");
            }
        }
        let _ = md::check_key(name);
    });
}

#[test]
fn fuzz_markdown() {
    markdown_never_panics(5000);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_markdown() {
    markdown_never_panics(5000 * DEEP);
}

#[test]
fn markdown_bombs_parse_in_bounded_time() {
    use atlas::markdown as md;
    let bombs: Vec<(&str, String)> = vec![
        ("deep list", (0..20_000).map(|i| format!("{}- item\n", "  ".repeat(i % 400))).collect()),
        ("giant table", "| a | b | c |\n|---|---|---|\n".to_string() + &"| x | [[y]] | z |\n".repeat(50_000)),
        ("unclosed fence", "```\n".to_string() + &"# not a heading [[x]]\n".repeat(50_000)),
        ("fence storm", "```\n~~~\n".repeat(50_000)),
        ("yaml nesting", "---\n".to_string() + &(0..5_000).map(|i| format!("{}k{i}:\n", " ".repeat(i % 200))).collect::<String>() + "---\nbody\n"),
        ("unclosed frontmatter", "---\n".to_string() + &"key: value\n".repeat(50_000)),
        ("wikilink storm", "[[".repeat(200_000)),
        ("bracket storm", "[".repeat(100_000) + &"](".repeat(100_000)),
        ("heading storm", "#".repeat(1_000_000)),
        ("one 10 MB line", "x".repeat(10 << 20)),
        ("cr only", "# a\r".repeat(100_000)),
        ("nul bytes", "\0# a\0\n[[\0]]\n".repeat(50_000)),
        ("tags", "#a/b/c ".repeat(100_000)),
    ];
    for (what, text) in &bombs {
        let t = Instant::now();
        let fm = md::parse_frontmatter(text);
        let _ = md::extract_links(text);
        let _ = md::extract_tags(text, &fm);
        let _ = md::headings(text);
        let _ = md::find_section(text, "a", None);
        let _ = atlas::chunk::chunks(text, &atlas::chunk::ChunkRule::default());
        assert!(t.elapsed() < Duration::from_secs(10), "{what} took {:?}", t.elapsed());
    }
}

#[test]
fn markdown_line_endings_and_boms() {
    use atlas::markdown as md;
    for nl in ["\n", "\r\n", "\r"] {
        let text = format!("\u{feff}---{nl}status: a{nl}---{nl}# H{nl}body{nl}");
        let mut up = Map::new();
        up.insert("status".into(), Value::from("b"));
        if let Ok(out) = md::set_frontmatter(&text, &up) {
            if nl != "\r" {
                assert!(!out.contains(if nl == "\n" { "\r\n" } else { "\n\n" }) || nl == "\r\n", "line endings kept: {out:?}");
            }
        }
        let _ = md::headings(&text);
        let _ = md::replace_section(&text, "H", None, "new");
    }
}

// ------------------------------------------------------------------ the rest

fn others_never_panic(n: usize) {
    use atlas::{chunk, code, digest, discussion, hook_rewrite, index, links, registry, text, wgsl};
    let shapes = ["cargo test", "cargo build", "git diff", "grep -n x", "cat a.rs", "ls", "", "rg x"];
    each(7, n, |r| (r.text_from(HOSTILE, 50), r.text_from(HOSTILE, 4), r.below(200)), |(t, s, k)| {
        for rule in [chunk::ChunkRule::default(), chunk::ChunkRule { top_level_items: true, max_level: 1 }, chunk::ChunkRule { top_level_items: true, max_level: 0 }] {
            let _ = chunk::chunks(t, &rule);
        }
        let _ = chunk::find_date(t);
        for line in t.lines().take(8) {
            let _ = chunk::heading(line);
            let _ = code::rust_item(line);
            let _ = code::rust_impl(line);
            let _ = registry::marker_ids(line);
            let _ = atlas::memory::import::index_target(line);
        }
        let c = chunk::cap(t, *k);
        assert!(c.len() <= t.len() && t.starts_with(c));
        assert_eq!(links::percent_decode(&links::percent_encode(t)), *t, "percent round trip");
        let _ = links::percent_decode(t);
        let _ = links::parse_uri(t);
        let _ = links::wikilink(t, Some(s), Some(s));
        let _ = links::stem(t);
        let _ = links::folder(t);
        let _ = links::escape_link_text(t);
        let _ = links::is_attachment(t);
        if let Some(i) = text::find_ci(t, s) {
            assert!(boundary(t, i), "find_ci returned a non-boundary {i}");
        }
        let _ = text::window(t, (*k).min(t.len()), 3, 40);
        let _ = text::window(t, *k, *k, *k);
        let _ = text::snippet(t, &[s.as_str()]);
        let _ = atlas::recall::words(t);
        let _ = registry::wikilink_target(t);
        let _ = code::rust_code_lines(t);
        let _ = code::scan_rust_defs(t);
        let _ = code::scan_rust_impls(t);
        let _ = wgsl::scan(t);
        let th = discussion::parse(t);
        let _ = discussion::verify(&th);
        let _ = discussion::status(&th);
        let _ = discussion::slug(t);
        let _ = discussion::check_body(t);
        let _ = discussion::check_author(s);
        for sh in shapes {
            let shape = digest::shape("bash", sh, t);
            let _ = digest::items(shape, t);
            let _ = digest::digest("h1", s, shape, t, Some(1), *k * 10);
        }
        let _ = digest::outline(t);
        let _ = digest::summary(t);
        let _ = digest::last_step(t);
        let _ = digest::lang_of(s);
        let _ = index::regions(t);
        let _ = index::check_glob(s);
        let _ = index::check_name(s);
        let _ = hook_rewrite::routable(t);
        for shell in ["bash", "powershell", "cmd", ""] {
            let _ = hook_rewrite::rewrite(shell, t);
            let _ = hook_rewrite::shell_read(shell, t);
            let _ = hook_rewrite::segments(shell, t, s, "/w");
        }
        let _ = hook_rewrite::unwrite(t, s);
        let _ = hook_rewrite::relative(t, s);
        let _ = atlas::memory::import::split_items(t);
        let _ = atlas::memory::import::split_headings(t);
        let _ = atlas::memory::import::split_index(t);
        let _ = atlas::memory::import::split_memory_file(t);
        let _ = atlas::memory::import::front_value(t, s);
        let words: Vec<String> = t.split(' ').map(str::to_string).collect();
        let _ = atlas::handles::parse_sel(&words);
        let _ = atlas::update::parse_version(t);
        let _ = atlas::update::parse_manifest(t.as_bytes());
        let _ = atlas::update::key_from_hex(t);
    });
}

#[test]
fn fuzz_text_parsers() {
    others_never_panic(2500);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_text_parsers() {
    others_never_panic(2500 * DEEP);
}

/// `natural_cmp` orders index rows: it must be a total order, or a sort
/// can panic ("user-provided comparison function does not correctly
/// implement a total order") on hostile note names.
fn natural_cmp_is_a_total_order(n: usize) {
    use atlas::index::natural_cmp;
    use std::cmp::Ordering;
    const T: &[&str] = &["a", "A", "1", "01", "001", "9", "10", "-", " ", "\u{e9}", "\u{130}", "\u{661}", "99999999999999999999999", "0"];
    each(8, n, |r| (r.text_from(T, 5), r.text_from(T, 5), r.text_from(T, 5)), |(a, b, c)| {
        assert_eq!(natural_cmp(a, a), Ordering::Equal);
        assert_eq!(natural_cmp(a, b), natural_cmp(b, a).reverse(), "antisymmetry {a:?} {b:?}");
        if natural_cmp(a, b) != Ordering::Greater && natural_cmp(b, c) != Ordering::Greater {
            assert_ne!(natural_cmp(a, c), Ordering::Greater, "transitivity {a:?} {b:?} {c:?}");
        }
    });
}

#[test]
fn fuzz_natural_order() {
    natural_cmp_is_a_total_order(20_000);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_natural_order() {
    natural_cmp_is_a_total_order(20_000 * DEEP);
}

fn binary_decoders_never_panic(n: usize) {
    use atlas::protobuf::{Reader, Writer};
    each(9, n, |r| {
        let mut w = Writer::default();
        for _ in 0..r.below(6) {
            match r.below(4) {
                0 => {
                    w.int(1 + r.below(8) as u32, r.next() as i64);
                }
                1 => {
                    let s = r.text_from(HOSTILE, 4);
                    w.string(1 + r.below(8) as u32, &s);
                }
                2 => {
                    let v: Vec<i32> = (0..r.below(5)).map(|_| r.next() as i32).collect();
                    w.packed(1 + r.below(8) as u32, &v);
                }
                _ => {
                    let mut inner = Writer::default();
                    inner.string(1, "scip-rust x 1 a/b#c().");
                    w.message(1 + r.below(8) as u32, &inner);
                }
            }
        }
        let mut b = w.buf.clone();
        if r.chance(2, 3) {
            r.mutate(&mut b);
        }
        b
    }, |b| {
        let mut rd = Reader::new(b);
        let mut guard = 0;
        while let Ok(Some((_, wt))) = rd.field() {
            if rd.skip(wt).is_err() {
                break;
            }
            guard += 1;
            assert!(guard <= b.len() + 1, "reader does not advance");
        }
        let _ = atlas::scip::decode(b, atlas::scip::Options::default());
        let _ = atlas::scip::decode(b, atlas::scip::Options { keep_locals: true });
    });
}

#[test]
fn fuzz_binary_decoders() {
    binary_decoders_never_panic(5000);
}

#[test]
#[ignore = "deep fuzz: cargo test --release --test adversarial -- --ignored"]
fn deep_fuzz_binary_decoders() {
    binary_decoders_never_panic(5000 * DEEP);
}
