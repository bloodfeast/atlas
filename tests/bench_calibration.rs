//! `atlas --bench` prints a pure-CPU calibration row at the start of the
//! run and after every section, in the shape of every other timing row, and
//! ends with the calibration summary; so a section that ran while the
//! machine was slow is seen in the report itself (DEPS2 stage 1, the
//! performance review of #248, finding 2).
#![cfg(feature = "dev")]

mod common;

use std::process::Command;

/// The points of a run on the fixture with a query set and more than one
/// cold run, in the order the bench reaches them.
const POINTS: [&str; 7] = ["start", "after cold index", "after rescans", "after lookup", "after ids, search and answers", "after memory store", "after queue and classify"];

#[test]
fn every_section_prints_a_calibration_row() {
    let t = common::Temp::new("bench-calibration");
    let db = t.path().join("atlas.sqlite");
    let out = Command::new(env!("CARGO_BIN_EXE_atlas"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["--config", "fixtures/generic/atlas.json", "--db"])
        .arg(&db)
        .args(["--bench", "10", "--queries", "fixtures/generic/queries.txt", "--cold-runs", "2"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "the bench failed: {text}\n{}", String::from_utf8_lossy(&out.stderr));
    let rows: Vec<&str> = text.lines().filter(|l| l.starts_with("calibration (pure CPU), ")).collect();
    let points: Vec<&str> = rows.iter().map(|l| l["calibration (pure CPU), ".len()..].split(" n=200").next().unwrap_or("").trim()).collect();
    assert_eq!(points, POINTS, "{text}");
    for row in &rows {
        // The shape of every timing row: `n=200  median  X us   p99  Y us`.
        let cells: Vec<&str> = row.split_whitespace().collect();
        let at = cells.iter().position(|c| *c == "n=200").unwrap_or_else(|| panic!("no n=200 in {row}"));
        assert_eq!((cells[at + 1], cells[at + 3], cells[at + 4], cells[at + 6]), ("median", "us", "p99", "us"), "{row}");
        let median: f64 = cells[at + 2].parse().unwrap();
        let p99: f64 = cells[at + 5].parse().unwrap();
        assert!(median > 0.0 && p99 >= median, "{row}");
    }
    // Each row follows its section: the cold index rows come before the
    // row that closes them, and the rescans after it.
    let line_of = |needle: &str| text.lines().position(|l| l.starts_with(needle)).unwrap_or_else(|| panic!("no line {needle}: {text}"));
    assert!(line_of("calibration (pure CPU), start") < line_of("cold index, open"));
    assert!(line_of("cold index, total") < line_of("calibration (pure CPU), after cold index"));
    assert!(line_of("calibration (pure CPU), after cold index") < line_of("rescan vaults, walk, reread paths"));
    assert!(line_of("rescan sources, walk, kept paths") < line_of("calibration (pure CPU), after rescans"));
    assert!(line_of("lookup JSON answer, after") < line_of("calibration (pure CPU), after lookup"));
    assert!(line_of("whole answer (watch, json)") < line_of("calibration (pure CPU), after ids, search and answers"));
    assert!(line_of("journal write") < line_of("calibration (pure CPU), after memory store"));
    assert!(line_of("classify a ") < line_of("calibration (pure CPU), after queue and classify"));
    let last = text.lines().last().unwrap_or("");
    assert!(last.starts_with("calibration: fastest ") && (last.contains("no row over 1.15x the fastest") || last.contains("SLOW (over 1.15x the fastest): ")), "{last}");
}
