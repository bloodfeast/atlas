//! PR #332 review r3: a long error line never hides the matches after it (M1),
//! the numeric level rule is pino's alone (S2), and `K shown` counts the
//! matches the tail shows too (N1). Shapes c1, c2, c7 and c9 are the review's.

use atlas::digest::{self, Shape};

const MAX: usize = 600;

fn generic_at(t: &str, max: usize) -> String {
    digest::digest("h:1", "cat a; tail b", Shape::Generic, t, None, max)
}

fn generic(t: &str) -> String {
    generic_at(t, MAX)
}

fn plain(from: usize, to: usize) -> String {
    (from..=to).map(|i| format!("line {i} of a plain build\n")).collect()
}

/// The numbered quote lines (`N: error...`) and the bytes they take, the
/// lines before the first `first ` head line only.
fn quotes(d: &str) -> (Vec<String>, usize) {
    let q: Vec<String> = d
        .lines()
        .take_while(|l| !l.starts_with("first "))
        .filter(|l| l.split_once(": ").is_some_and(|(n, _)| n.parse::<usize>().is_ok()))
        .map(String::from)
        .collect();
    let bytes = q.iter().map(|l| l.len() + 1).sum();
    (q, bytes)
}

/// The half-raw rule and the quote budget: the digest past its label line
/// is at most half the raw text, and the quotes take at most half of `max`.
fn assert_bounds(t: &str, d: &str, max: usize) {
    let body = d.split_once('\n').map_or(d, |(_, b)| b);
    assert!(body.len() <= t.len() / 2, "raw {} B, body {} B: {d}", t.len(), body.len());
    let (_, bytes) = quotes(d);
    assert!(bytes <= max / 2, "quotes {bytes} B over {} B: {d}", max / 2);
}

/// c9: a Cyrillic error line over the quote budget, then five short ones.
fn c9(ch: char) -> String {
    let mut t = plain(1, 100);
    t.push_str(&format!("error: {}\n", ch.to_string().repeat(150)));
    for i in 1..=5 {
        t.push_str(&format!("error: unit {i}\n"));
    }
    t.push_str(&plain(107, 506));
    t
}

#[test]
fn a_long_multibyte_error_line_does_not_hide_the_short_ones() {
    for ch in ['\u{436}', '\u{4e2d}'] {
        let t = c9(ch);
        let d = generic(&t);
        eprintln!("c9 {ch}: raw {} B, digest {} B, quotes {:?}", t.len(), d.len(), quotes(&d).0.len());
        for i in 1..=5 {
            assert!(d.contains(&format!("\n{}: error: unit {i}\n", 101 + i)), "unit {i}: {d}");
        }
        assert!(d.contains("\n6 lines match error|fail|panic, 5 shown"), "{d}");
        assert_bounds(&t, &d, MAX);
    }
}

#[test]
fn c1_the_short_match_after_a_long_one_is_quoted() {
    let mut t = plain(1, 100);
    t.push_str(&format!("error: {}\nerror: x\n", "\u{436}".repeat(150)));
    t.push_str(&plain(103, 700));
    let d = generic(&t);
    eprintln!("c1: raw {} B, digest {} B", t.len(), d.len());
    assert!(d.contains("\n102: error: x\n"), "{d}");
    assert!(d.contains("\n2 lines match error|fail|panic, 1 shown"), "{d}");
    assert_bounds(&t, &d, MAX);
}

/// c2: a lone error line too long for the budget is quoted, clipped by
/// bytes on a char boundary and marked.
#[test]
fn c2_a_lone_oversize_match_is_quoted_clipped() {
    for ch in ['\u{4e2d}', '\u{436}', 'x'] {
        let mut t = plain(1, 100);
        t.push_str(&format!("error: {}\n", ch.to_string().repeat(400)));
        t.push_str(&plain(102, 700));
        for max in [MAX, 200] {
            let d = generic_at(&t, max);
            eprintln!("c2 {ch} max {max}: raw {} B, digest {} B", t.len(), d.len());
            let (q, _) = quotes(&d);
            assert_eq!(q.len(), 1, "{d}");
            assert!(q[0].starts_with("101: error: ") && q[0].ends_with("..."), "{d}");
            assert!(q[0].contains(ch), "{d}");
            assert!(d.contains("\n1 lines match error|fail|panic\n") || d.contains("\n1 lines match error|fail|panic;"), "{d}");
            assert_bounds(&t, &d, max);
        }
    }
}

/// c7: matches the tail shows count as shown.
#[test]
fn c7_matches_in_the_tail_count_as_shown() {
    let mut t = plain(1, 200);
    for i in 1..=30 {
        t.push_str(&format!("error: case {i}\n"));
    }
    let d = generic(&t);
    let seen: std::collections::BTreeSet<usize> =
        d.lines().filter_map(|l| l.split_once(": ").filter(|(_, r)| r.starts_with("error")).and_then(|(n, _)| n.parse().ok())).collect();
    eprintln!("c7: raw {} B, digest {} B, {} distinct matches visible", t.len(), d.len(), seen.len());
    assert!(seen.len() > 10, "{d}");
    let k = seen.len();
    if k < 30 {
        assert!(d.contains(&format!("\n30 lines match error|fail|panic, {k} shown")), "{d}");
    } else {
        assert!(!d.contains(" shown"), "{d}");
    }
}

fn records(n: usize, rec: impl Fn(usize) -> String) -> String {
    (0..n).map(|i| format!("{}\n", rec(i))).collect()
}

/// S2: only `level` is numeric, and only 50..=60 (pino's error and fatal).
#[test]
fn the_numeric_level_is_pinos_alone() {
    let d = generic(&records(300, |i| format!(r#"{{"severity":200,"message":"request {i} served"}}"#)));
    assert!(d.contains("\n0 lines match error|fail|panic"), "GCP INFO: {d}");
    let d = generic(&records(300, |i| format!(r#"{{"player":"p{i}","level":120}}"#)));
    assert!(d.contains("\n0 lines match error|fail|panic"), "game level: {d}");
    for bad in [r#""severity":500"#, r#""severity":50"#,r#""lvl":50"#, r#""level":61"#, r#""level":49"#, r#""level":50.5"#, r#""level":-50"#] {
        let t = records(300, |i| if i == 150 { format!("{{{bad},\"msg\":\"m\"}}") } else { format!(r#"{{"level":30,"msg":"step {i}"}}"#) });
        let d = generic(&t);
        assert!(d.contains("\n0 lines match error|fail|panic"), "{bad}: {d}");
    }
    for good in [r#""level":50"#, r#""level":55"#, r#""level":60"#] {
        let t =
            records(300, |i| if i == 150 { format!("{{{good},\"msg\":\"m\"}}") } else { format!(r#"{{"level":30,"msg":"step {i}"}}"#) });
        let d = generic(&t);
        assert!(d.contains("\n1 lines match error|fail|panic"), "{good}: {d}");
    }
}
