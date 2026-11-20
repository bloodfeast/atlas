//! The generic digest of a short output keeps every line (#326), any longer
//! one shows its head beside its tail, and JSONL records are one class.

use atlas::digest::{self, Shape};

/// The #326 shape: one short result line, then 15 multi-KB JSONL records
/// whose strings say "error" and "fail".
fn issue_326() -> String {
    let mut t = String::from("1054 MERGED d504f764\n");
    let pad = "x".repeat(700);
    for i in 0..15 {
        t.push_str(&format!("{{\"parentUuid\":\"u{i}\",\"type\":\"assistant\",\"text\":\"no error here, nothing to fail {pad}\"}}\n"));
    }
    t
}

fn generic(t: &str) -> String {
    digest::digest("h:1", "cat a; tail b", Shape::Generic, t, None, 600)
}

#[test]
fn the_326_shape_keeps_its_first_line() {
    let t = issue_326();
    assert!(t.len() > 10_240 && t.lines().count() == 16);
    let d = generic(&t);
    eprintln!("#326 shape: raw {} B, digest {} B", t.len(), d.len());
    assert!(d.contains("\n1: 1054 MERGED d504f764\n"), "{d}");
    // Records are clipped to 80 chars in the short path.
    assert!(d.len() < 2000, "{d}");
    for i in 2..=16 {
        assert!(d.contains(&format!("\n{i}: {{\"parentUuid\"")), "line {i}: {d}");
    }
}

#[test]
fn a_short_output_keeps_every_line_clipped() {
    let t: String = (1..=20).map(|i| format!("{i:03}{}\n", "y".repeat(497))).collect();
    let d = generic(&t);
    for i in 1..=20 {
        let want = format!("\n{i}: {i:03}{}...(+380 chars)\n", "y".repeat(117));
        assert!(d.contains(&want), "line {i}: {d}");
    }
    // 25 lines is no longer short: lines are dropped again.
    let t: String = (1..=25).map(|i| format!("{i:03}{}\n", "y".repeat(497))).collect();
    assert!(!generic(&t).contains("\n10: "));
}

#[test]
fn jsonl_records_are_one_class_and_never_match() {
    let mut t = String::new();
    for i in 1..=60 {
        if i == 30 {
            t.push_str("error: the real one\n");
        } else if i % 2 == 0 {
            t.push_str(&format!("{{\"i\":{i},\"msg\":\"an error and a failure\"}}\n"));
        } else {
            t.push_str(&format!("plain line {i}\n"));
        }
    }
    let d = generic(&t);
    assert!(d.contains("\n1 lines match error|fail|panic; exit status unknown\n"), "{d}");
    assert!(d.contains("\n29 JSONL records (matched by level, not text)\n"), "{d}");
    assert!(d.contains("\n30: error: the real one\n"), "{d}");
    // A tiny output counts them too and still matches its plain line,
    // though at 37 B no quote fits half of it, and the header says so.
    let d = generic("{\"e\":\"error\"}\n{\"e\":\"fail\"}\nfailed: x\n");
    assert!(d.contains("\n1 lines match error|fail|panic, 0 shown; exit status unknown\n2 JSONL records (matched by level, not text)\n"), "{d}");
    // Not an object, or not JSON: not a record.
    let d = generic("[\"error\"]\n{error}\n");
    assert!(d.contains("\n2 lines match") && !d.contains("JSONL"), "{d}");
}

/// A long plain log: its digest before #326, which the change keeps but for
/// the head block it adds and the two tail lines that make room for it.
const LONG_BEFORE: &str = "h:1 cat a; tail b, 100 lines, 2678 B
1 lines match error|fail|panic; exit status unknown
50: error: unit 50 broke
last 14 of 100 lines:
87: step 87: compiling unit 87
88: step 88: compiling unit 88
89: step 89: compiling unit 89
90: step 90: compiling unit 90
91: step 91: compiling unit 91
92: step 92: compiling unit 92
93: step 93: compiling unit 93
94: step 94: compiling unit 94
95: step 95: compiling unit 95
96: step 96: compiling unit 96
97: step 97: compiling unit 97
98: step 98: compiling unit 98
99: step 99: compiling unit 99
100: step 100: compiling unit 100
recall h:1 lines=A-B | grep=RE
";

fn long_plain() -> String {
    (1..=100).map(|i| if i == 50 { "error: unit 50 broke\n".to_string() } else { format!("step {i}: compiling unit {i}\n") }).collect()
}

#[test]
fn a_long_output_shows_its_head_and_is_otherwise_unchanged() {
    let d = generic(&long_plain());
    let head = "first 2 of 100 lines:\n1: step 1: compiling unit 1\n2: step 2: compiling unit 2\n";
    assert!(d.contains(&format!("{head}last ")), "{d}");
    assert_eq!(d.replacen(head, "", 1), LONG_BEFORE);
}
