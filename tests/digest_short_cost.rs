//! The generic digest's cost bounds and JSON error levels (#326 follow-ups,
//! PR #332 review r1: S1, S2 and the nits).

use atlas::digest::{self, Shape};

fn generic(t: &str) -> String {
    digest::digest("h:1", "cat a; tail b", Shape::Generic, t, None, 600)
}

fn lines(n: usize, width: usize) -> String {
    (1..=n).map(|i| format!("{i:03}{}\n", "y".repeat(width - 3))).collect()
}

#[test]
fn exactly_24_lines_is_short_and_25_is_long() {
    let d = generic(&lines(24, 500));
    assert!(d.contains("\n12: 012") && !d.contains("first 2 of"), "{d}");
    let d = generic(&lines(25, 500));
    assert!(!d.contains("\n12: 012") && d.contains("first 2 of 25 lines:"), "{d}");
}

#[test]
fn the_short_path_is_taken_only_at_half_the_raw_size() {
    // 24 short lines: every line kept would cost more than half the raw text.
    let t = lines(24, 30);
    let d = generic(&t);
    assert!(d.contains("first 2 of 24 lines:"), "{d}");
}

#[test]
fn the_worst_short_output_has_a_ceiling() {
    let t = lines(24, 5000);
    let d = generic(&t);
    eprintln!("short ceiling: raw {} B, digest {} B", t.len(), d.len());
    assert!(d.contains("\n24: 024") && d.len() <= 3600, "{} B: {d}", d.len());
}

/// At the lowest `lean.handles.threshold_bytes` (1024) no shape of output
/// gets a digest larger than itself.
#[test]
fn a_digest_is_never_larger_than_its_output_at_1024_bytes() {
    let fill = |mut t: String| {
        let mut i = 0;
        while t.len() < 1024 {
            t.push_str(&format!("m{i}\n"));
            i += 1;
        }
        t
    };
    let long_head = "h".repeat(400);
    let err = format!("error: {}", "e".repeat(140));
    let mut shapes = vec![
        lines(24, 42),
        lines(25, 41),
        fill("x\n".repeat(30)),
        fill(format!("{long_head}\n{long_head}\n{}", format!("{err}\n").repeat(3))),
        fill(format!("{}\n{}\n", "\u{e9}".repeat(300), "\u{4e2d}".repeat(150))),
        fill(format!("{}{}", format!("{err}\n").repeat(4), "a\n".repeat(30))),
    ];
    let mut t = format!("{long_head}\n{long_head}\n");
    for _ in 0..30 {
        t.push_str("error z\n");
    }
    shapes.push(fill(t));
    for t in shapes {
        assert!(t.len() >= 1024);
        let d = generic(&t);
        assert!(d.len() < t.len(), "raw {} B, digest {} B: {d}", t.len(), d.len());
        assert!(d.contains("\n1: "), "{d}");
        assert_matches_quoted(&t, &d);
    }
}

/// The numbered lines of a digest that quote an `error` line.
fn quoted_matches(d: &str) -> usize {
    d.lines()
        .filter(|l| l.split_once(": ").is_some_and(|(n, r)| n.parse::<usize>().is_ok() && r.starts_with("error")))
        .count()
}

/// A digest of an output with `error` lines quotes at least one, and its
/// header says how many it shows when it shows fewer than match.
fn assert_matches_quoted(t: &str, d: &str) {
    let n = t.lines().filter(|l| l.starts_with("error")).count();
    if n == 0 {
        return;
    }
    let k = quoted_matches(d);
    assert!(k >= 1, "{n} matches, none quoted: {d}");
    if k < n {
        assert!(d.contains(&format!("\n{n} lines match error|fail|panic, {k} shown")), "{n} matches, {k} quoted: {d}");
    } else {
        assert!(d.contains(&format!("\n{n} lines match error|fail|panic")) && !d.contains(" shown"), "{d}");
    }
}

/// The quotes take half the 600 B budget: two 150 B quotes fill it exactly
/// and the third is left out, which the header says.
#[test]
fn quotes_fill_half_the_budget_exactly() {
    // "NN: " + 145 chars + "\n" is 150 B.
    let err = format!("error: {}", "q".repeat(138));
    let t: String = (1..=300)
        .map(|i| if (10..=12).contains(&i) { format!("{err}\n") } else { format!("line {i} of a plain build\n") })
        .collect();
    let d = generic(&t);
    assert!(d.contains("\n10: error") && d.contains("\n11: error") && !d.contains("\n12: error"), "{d}");
    assert_eq!(d.lines().filter(|l| l.starts_with("10: ")).map(str::len).sum::<usize>() + 1, 150, "{d}");
    assert!(d.contains("\n3 lines match error|fail|panic, 2 shown"), "{d}");
}

/// PR #332 review r2, M1: the last fallback kept line 1 and dropped every
/// quoted match. Before -> after bytes go to stderr.
#[test]
fn the_last_fallback_still_quotes_its_matches() {
    let fill = |mut t: String, to: usize| {
        let mut i = 0;
        while t.len() < to {
            t.push_str(&format!("m{i}\n"));
            i += 1;
        }
        t
    };
    let h300 = "h".repeat(300);
    let errs: String = (1..=20).map(|i| format!("error: unit {i} broke\n")).collect();
    let emoji = "\u{1f600}".repeat(160);
    let filler: String = (1..=22).map(|i| format!("f{i}\n")).collect();
    let h400 = "h".repeat(400);
    for (t, d) in [
        fill(format!("{h300}\n{h300}\n{errs}"), 1024),
        format!("{emoji}\n{emoji}\n{filler}error: the one that matters\n"),
        fill(format!("{h400}\n{h400}\n{}", "error z\n".repeat(30)), 1024),
    ]
    .map(|t| {
        let d = generic(&t);
        eprintln!("r2 repro: raw {} B, digest {} B, {} quoted", t.len(), d.len(), quoted_matches(&d));
        (t, d)
    }) {
        assert!(d.len() <= t.len() / 2, "raw {} B, digest {} B: {d}", t.len(), d.len());
        assert!(d.contains("first 1 of ") && d.contains("\n1: "), "{d}");
        assert_matches_quoted(&t, &d);
    }
}

fn json_log(bad: &str) -> String {
    (1..=500).map(|i| if i == 250 { format!("{bad}\n") } else { format!("{{\"level\":\"INFO\",\"msg\":\"step {i} ok\"}}\n") }).collect()
}

#[test]
fn a_json_record_at_error_level_is_a_match() {
    for bad in [
        r#"{"level":"ERROR","msg":"db down"}"#,
        r#"{"severity":"Fatal","msg":"db down"}"#,
        r#"{"lvl":"panic","msg":"db down"}"#,
        r#"{"type":"result","is_error":true,"msg":"db down"}"#,
        // PR #332 review r2, S2: pino/bunyan numbers, syslog and GCP words,
        // and `is_error` as a string.
        r#"{"level":50,"msg":"db down"}"#,
        r#"{"level":60,"msg":"db down"}"#,
        r#"{"severity":"CRITICAL","msg":"db down"}"#,
        r#"{"level":"crit","msg":"db down"}"#,
        r#"{"level":"Err","msg":"db down"}"#,
        r#"{"severity":"EMERGENCY","msg":"db down"}"#,
        r#"{"level":"emerg","msg":"db down"}"#,
        r#"{"severity":"Alert","msg":"db down"}"#,
        r#"{"type":"result","is_error":"true","msg":"db down"}"#,
    ] {
        let d = generic(&json_log(bad));
        assert!(d.contains("\n1 lines match error|fail|panic"), "{bad}: {d}");
        assert!(d.contains(&format!("\n250: {bad}\n")), "{bad}: {d}");
    }
}

#[test]
fn error_words_elsewhere_in_a_record_do_not_match() {
    for ok in [
        r#"{"level":"info","msg":"error rate 0, nothing to fail"}"#,
        r#"{"msg":"level error"}"#,
        r#"{"inner":{"level":"error"}}"#,
        r#"{"is_error":false,"level":"warn"}"#,
        r#"{"level":"errors"}"#,
        r#"{"level":40,"msg":"warn"}"#,
        r#"{"level":"50"}"#,
        r#"{"level":"critic"}"#,
        r#"{"is_error":"false"}"#,
        r#"{"inner":{"level":50}}"#,
        r#"{"level":{"name":"error"}}"#,
    ] {
        let d = generic(&json_log(ok));
        assert!(d.contains("\n0 lines match error|fail|panic"), "{ok}: {d}");
    }
}

#[test]
fn a_match_in_the_head_is_quoted_once() {
    let t: String = (1..=60).map(|i| if i == 1 { "error: first\n".to_string() } else { format!("line {i}\n") }).collect();
    let d = generic(&t);
    assert_eq!(d.matches("1: error: first\n").count(), 1, "{d}");
    assert!(d.contains("first 2 of 60 lines:\n1: error: first\n2: line 2\n"), "{d}");
}

#[test]
fn the_head_says_what_it_clipped_and_counts_in_the_budget() {
    let head = "h".repeat(300);
    let t: String = (1..=100)
        .map(|i| if i <= 2 { format!("{head}\n") } else { format!("step {i}: compiling unit {i} of the long plain build\n") })
        .collect();
    let d = generic(&t);
    assert!(d.contains(&format!("\n1: {}...(+140 chars)\n", "h".repeat(160))), "{d}");
    // The tail gives way to the head: the digest stays near the 600 B budget.
    assert!(d.len() <= 600 + 200, "{} B: {d}", d.len());
}

#[test]
fn a_multibyte_line_is_clipped_on_a_char_boundary() {
    let t: String = (1..=10).map(|_| format!("{}\n", "\u{e9}".repeat(1000))).collect();
    let d = generic(&t);
    assert!(d.contains(&format!("\n1: {}...(+880 chars)\n", "\u{e9}".repeat(120))), "{d}");
}

#[test]
fn crlf_lines_keep_no_carriage_return() {
    let t: String = (1..=10).map(|i| format!("{i:03}{}\r\n", "c".repeat(1000))).collect();
    let d = generic(&t);
    assert!(!d.contains('\r') && d.contains("\n10: 010"), "{d}");
}
