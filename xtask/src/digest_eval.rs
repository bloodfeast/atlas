//! `cargo xtask digest-eval`: the digest fidelity eval.
//!
//! atlas puts a digest of a long output into context in place of the
//! output, and a slice when the agent asks for more. A digest cut is only
//! safe if the line that explains a failure still reaches the agent, and
//! that line is often not the panic: it can be a warning, a setup line or a
//! build-script message far from where the run gave up. This scores the
//! digest on a corpus of real failing runs whose root-cause lines were
//! labelled by hand:
//!
//! * `digest`: every labelled cause is in the digest alone;
//! * `slice`: not in the digest, but in the digest plus the one follow-up
//!   slice the policy below picks from what the digest showed;
//! * `missed`: in neither.
//!
//! Tokens are the bytes of the digest, of the slice and of the raw output
//! over [`atlas::tokens::DEFAULT_BYTES_PER_TOKEN`], as the memory eval
//! counts them. Each output is stored and answered through the real
//! [`atlas::handles::Handles`] (a fresh store in a temporary folder), so the
//! digest and the slice are the ones `atlas run` and `atlas slice` give.
//!
//! The follow-up policy (one slice, chosen by rule, never from the label):
//!
//! * a test run or a check log with a totals table (`cargo test`, nextest,
//!   the other runners, `cargo xtask check`): `--failures`;
//! * a cargo build with errors: `--item error:1`; without: `--tail 40`;
//! * any other output: the 21 lines around the first line the digest quotes
//!   as matching error|fail|panic, else `--head 40`.
//!
//! The case file is plain text, one block per case:
//!
//! ```text
//! D001 cargo-test
//! file: runs/D001.txt
//! cmd: cargo test -p engine --release
//! exit: 101
//! from: CI run 12345, the release job's log
//! commit: 8d32844
//! cause: 1107 | did not panic as expected
//! why: the test expects a debug assertion that a release build compiles out
//! middle: no
//! ```
//!
//! `file` is relative to the case file's folder. `exit` is the exit code, or
//! `none` when the run's capture did not keep it (the digest then infers
//! pass or fail, as the hook does). `cause: LINE | TEXT` (one or more) names
//! a root-cause line of the raw output and a distinctive part of it; TEXT
//! must occur on that line, or the case is invalid and the run fails. A
//! case is caught at a level when every cause's TEXT occurs in what that
//! level returned. `middle: yes` marks a cause that is not the failure line
//! itself (a warning, a setup or build-script line away from the panic):
//! the cases a digest is most likely to drop.
//!
//! Output: a summary line, a table by runner, a `miss` line per case not
//! caught at either level; `--rows` adds a `row` line per case; `--show ID`
//! prints a case's digest, follow-up and slice. `--gate ROWS` reads the
//! `row` lines of an earlier run and fails if any case is caught at a worse
//! level now, or if fewer cases are caught at digest or slice level in all
//! (Q-DE3: a digest cut is gated on it, as memory cuts are on the memory
//! eval).
//!
//! The corpus is private and lives outside the repository, as the memory
//! eval's question set does: the runs are a user's own logs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use atlas::digest::Shape;
use atlas::handles::{HandleCfg, Handles, Meta, Sel};

/// One labelled failing run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Case {
    /// `D001`.
    pub id: String,
    /// The runner, as the report groups it (`cargo-test`, `nextest`, `go-test`).
    pub runner: String,
    /// The raw output, relative to the case file's folder.
    pub file: String,
    /// The command that produced it.
    pub cmd: String,
    /// Its exit code, if the capture kept it.
    pub exit: Option<i32>,
    /// Where the output came from.
    pub from: String,
    /// The commit it ran at, when known.
    pub commit: String,
    /// Root-cause lines: (1-based line, text that must appear).
    pub causes: Vec<(usize, String)>,
    /// Why those lines are the cause.
    pub why: String,
    /// The cause is away from the failure line.
    pub middle: bool,
}

/// Parses a case file.
///
/// # Errors
///
/// A message naming the line for a field outside a block, an unknown
/// field, a bad number, or a block missing `file`, `cmd`, `from`, `cause`
/// or `why`.
pub fn parse(text: &str) -> Result<Vec<Case>, String> {
    let mut out: Vec<Case> = Vec::new();
    let mut cur: Option<Case> = None;
    fn finish(c: Option<Case>, out: &mut Vec<Case>) -> Result<(), String> {
        if let Some(c) = c {
            if c.file.is_empty() || c.cmd.is_empty() || c.from.is_empty() || c.causes.is_empty() || c.why.is_empty() {
                return Err(format!("{}: needs file, cmd, from, cause and why", c.id));
            }
            if out.iter().any(|o| o.id == c.id) {
                return Err(format!("{}: a second case with this id", c.id));
            }
            out.push(c);
        }
        Ok(())
    }
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |m: &str| format!("line {}: {m}", n + 1);
        if let Some((k, v)) = line.split_once(": ").filter(|(k, _)| !k.contains(' ')) {
            let c = cur.as_mut().ok_or_else(|| at("a field outside a case"))?;
            let v = v.trim();
            match k {
                "file" => c.file = v.into(),
                "cmd" => c.cmd = v.into(),
                "exit" => c.exit = if v == "none" { None } else { Some(v.parse().map_err(|_| at("exit: a number or none"))?) },
                "from" => c.from = v.into(),
                "commit" => c.commit = v.into(),
                "why" => c.why = v.into(),
                "middle" => {
                    c.middle = match v {
                        "yes" => true,
                        "no" => false,
                        _ => return Err(at("middle: yes or no")),
                    }
                }
                "cause" => {
                    let (l, t) = v.split_once(" | ").ok_or_else(|| at("cause: LINE | TEXT"))?;
                    let l: usize = l.trim().parse().map_err(|_| at("cause: LINE | TEXT"))?;
                    if l == 0 || t.trim().is_empty() {
                        return Err(at("cause: LINE | TEXT"));
                    }
                    c.causes.push((l, t.trim().to_string()));
                }
                _ => return Err(at(&format!("unknown field {k:?}"))),
            }
        } else {
            let mut w = line.split_whitespace();
            let (Some(id), Some(runner), None) = (w.next(), w.next(), w.next()) else {
                return Err(at("expected `ID RUNNER` or `field: value`"));
            };
            finish(cur.take(), &mut out)?;
            cur = Some(Case { id: id.into(), runner: runner.into(), ..Case::default() });
        }
    }
    finish(cur, &mut out)?;
    Ok(out)
}

/// Checks a case's labels against its raw output: every cause line exists
/// and holds its text.
///
/// # Errors
///
/// A message naming the case and the cause.
pub fn validate(c: &Case, raw: &str) -> Result<(), String> {
    let lines: Vec<&str> = raw.lines().collect();
    for (l, t) in &c.causes {
        let Some(line) = lines.get(l - 1) else {
            return Err(format!("{}: cause line {l} is past the end ({} lines)", c.id, lines.len()));
        };
        if !line.contains(t.as_str()) {
            return Err(format!("{}: cause line {l} does not hold {t:?}", c.id));
        }
    }
    Ok(())
}

/// How far an agent reading the digest gets to the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// In the digest alone.
    Digest,
    /// In the digest plus the one follow-up slice.
    Slice,
    /// In neither.
    Missed,
}

impl Level {
    /// The word the rows print.
    pub fn word(self) -> &'static str {
        match self {
            Level::Digest => "digest",
            Level::Slice => "slice",
            Level::Missed => "missed",
        }
    }

    fn of(w: &str) -> Option<Level> {
        match w {
            "digest" => Some(Level::Digest),
            "slice" => Some(Level::Slice),
            "missed" => Some(Level::Missed),
            _ => None,
        }
    }
}

/// The level a case is caught at, from what each step returned.
pub fn level(causes: &[(usize, String)], digest: &str, slice: &str) -> Level {
    if causes.iter().all(|(_, t)| digest.contains(t.as_str())) {
        Level::Digest
    } else if causes.iter().all(|(_, t)| digest.contains(t.as_str()) || slice.contains(t.as_str())) {
        Level::Slice
    } else {
        Level::Missed
    }
}

/// The follow-up slice the policy picks from the digest (see the module
/// docs), with its words for the report.
pub fn follow_up(shape: Shape, digest: &str) -> (Sel, String) {
    match shape {
        Shape::CargoTest | Shape::Test(_) | Shape::Summary => (Sel::Failures, "--failures".into()),
        Shape::CargoBuild => {
            if digest.lines().any(|l| l.starts_with("error:1 ")) {
                (Sel::Item("error:1".into()), "--item error:1".into())
            } else {
                (Sel::Tail(40), "--tail 40".into())
            }
        }
        _ => {
            // The generic digest quotes its matching lines as `N: text`
            // between its count line and the `last K of N lines:` tail.
            let first = digest
                .lines()
                .skip(1)
                .take_while(|l| !l.starts_with("last "))
                .find_map(|l| l.split_once(": ").and_then(|(n, _)| n.parse::<usize>().ok()));
            match first {
                Some(h) => {
                    let (a, b) = (h.saturating_sub(10).max(1), h + 10);
                    (Sel::Lines(a, b), format!("--lines {a}-{b}"))
                }
                None => (Sel::Head(40), "--head 40".into()),
            }
        }
    }
}

/// One scored case.
#[derive(Debug, Clone)]
pub struct Scored {
    /// The case.
    pub case: Case,
    /// Its level.
    pub level: Level,
    /// The follow-up's words.
    pub follow: String,
    /// The shape atlas read the output as.
    pub shape: String,
    /// Bytes of the digest, the slice and the raw output.
    pub bytes: [usize; 3],
    /// The digest and the slice (for `--show`).
    pub texts: [String; 2],
}

fn shape_word(s: Shape) -> String {
    match s {
        Shape::CargoTest => "cargo-test".into(),
        Shape::Test(r) => r.name.replace(' ', "-"),
        Shape::CargoBuild => "cargo-build".into(),
        Shape::Grep => "grep".into(),
        Shape::Listing => "listing".into(),
        Shape::Source(_) => "source".into(),
        Shape::Summary => "summary".into(),
        Shape::Generic => "generic".into(),
    }
}

/// Scores one case through a handle store.
///
/// # Errors
///
/// A message from the store.
pub fn score(h: &mut Handles, c: &Case, raw: &[u8]) -> Result<Scored, String> {
    let m = Meta { kind: "run".into(), label: c.cmd.clone(), cmd: c.cmd.clone(), exit_code: c.exit.map(i64::from), ..Meta::default() };
    let row = h.put(raw, &m).map_err(|e| format!("{}: {e}", c.id))?;
    let d = h.digest(&row.id).map_err(|e| format!("{}: {e}", c.id))?;
    let text = String::from_utf8_lossy(raw);
    let shape = Handles::shape_of(&row, &text);
    let (sel, follow) = follow_up(shape, &d);
    let s = h.slice(&row.id, &sel).map_err(|e| format!("{}: {e}", c.id))?;
    Ok(Scored {
        case: c.clone(),
        level: level(&c.causes, &d, &s),
        follow,
        shape: shape_word(shape),
        bytes: [d.len(), s.len(), raw.len()],
        texts: [d, s],
    })
}

/// The options of a run.
#[derive(Debug, Clone, Default)]
pub struct Opts {
    /// The case file.
    pub cases: PathBuf,
    /// Print a `row` line per case.
    pub rows: bool,
    /// Cases to print in full.
    pub show: Vec<String>,
    /// An earlier run's rows to gate against.
    pub gate: Option<PathBuf>,
}

/// Totals of a group of cases.
#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    n: usize,
    at: [usize; 3],
    bytes: [usize; 3],
}

impl Tally {
    fn add(&mut self, s: &Scored) {
        self.n += 1;
        self.at[s.level as usize] += 1;
        for k in 0..3 {
            self.bytes[k] += s.bytes[k];
        }
    }
}

fn tok(b: usize) -> f64 {
    b as f64 / atlas::tokens::DEFAULT_BYTES_PER_TOKEN
}

/// The report's lines: a summary, the table by runner, a line per miss,
/// and with `rows` a line per case. Stable for a given corpus and atlas.
pub fn report(scored: &[Scored], rows: bool) -> String {
    let mut by: BTreeMap<&str, Tally> = BTreeMap::new();
    let (mut all, mut mid) = (Tally::default(), Tally::default());
    for s in scored {
        by.entry(s.case.runner.as_str()).or_default().add(s);
        all.add(s);
        if s.case.middle {
            mid.add(s);
        }
    }
    let mut out = format!(
        "digest-eval: {} cases, {} runners; digest {}, digest+slice {}, missed {}; middle cases {}: digest {}, digest+slice {}, missed {}\n",
        all.n,
        by.len(),
        all.at[0],
        all.at[0] + all.at[1],
        all.at[2],
        mid.n,
        mid.at[0],
        mid.at[0] + mid.at[1],
        mid.at[2],
    );
    out.push_str(&format!(
        "tokens: digest {:.0}, slice {:.0}, raw {:.0}; digest+slice is {:.1}% of raw\n",
        tok(all.bytes[0]),
        tok(all.bytes[1]),
        tok(all.bytes[2]),
        100.0 * (all.bytes[0] + all.bytes[1]) as f64 / all.bytes[2].max(1) as f64
    ));
    out.push_str("runner\tcases\tdigest\tslice\tmissed\ttok_digest\ttok_slice\ttok_raw\n");
    let line = |name: &str, t: &Tally| {
        format!("{name}\t{}\t{}\t{}\t{}\t{:.0}\t{:.0}\t{:.0}\n", t.n, t.at[0], t.at[1], t.at[2], tok(t.bytes[0]), tok(t.bytes[1]), tok(t.bytes[2]))
    };
    for (name, t) in &by {
        out.push_str(&line(name, t));
    }
    out.push_str(&line("all", &all));
    for s in scored.iter().filter(|s| s.level == Level::Missed) {
        out.push_str(&format!("miss\t{}\t{}\t{}\t{}\n", s.case.id, s.case.runner, if s.case.middle { "middle" } else { "-" }, s.case.why));
    }
    if rows {
        for s in scored {
            out.push_str(&format!(
                "row\t{}\t{}\t{}\t{}\t{}\t{}\t{:.0}\t{:.0}\t{:.0}\n",
                s.case.id,
                s.case.runner,
                s.shape,
                s.level.word(),
                if s.case.middle { "middle" } else { "-" },
                s.follow,
                tok(s.bytes[0]),
                tok(s.bytes[1]),
                tok(s.bytes[2])
            ));
        }
    }
    out
}

/// What fails the gate against an earlier run's `row` lines (Q-DE3): each
/// case caught at a worse level than it was (a case the earlier run did not
/// have is not compared), and the digest-plus-slice total falling.
pub fn regressions(earlier: &str, scored: &[Scored]) -> Vec<String> {
    let before: BTreeMap<&str, Level> = earlier
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() > 4 && f[0] == "row").then(|| Level::of(f[4]).map(|lv| (f[1], lv))).flatten()
        })
        .collect();
    let mut out: Vec<String> = scored
        .iter()
        .filter_map(|s| {
            let b = before.get(s.case.id.as_str())?;
            (s.level > *b).then(|| format!("{}: {} -> {}", s.case.id, b.word(), s.level.word()))
        })
        .collect();
    let was = before.values().filter(|l| **l != Level::Missed).count();
    let now = scored.iter().filter(|s| s.level != Level::Missed).count();
    if now < was {
        out.push(format!("digest+slice total: {was} -> {now}"));
    }
    out
}

/// Runs the eval and prints its report. Returns false when the gate found
/// a regression.
///
/// # Errors
///
/// An unreadable or invalid case file or output, or a store error.
pub fn run(o: &Opts) -> Result<bool, String> {
    let text = std::fs::read_to_string(&o.cases).map_err(|e| format!("{}: {e}", o.cases.display()))?;
    let cases = parse(&text)?;
    let base = o.cases.parent().unwrap_or(Path::new("."));
    let tmp = std::env::temp_dir().join(format!("atlas-digest-eval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let mut h = Handles::open(&tmp.join("index.sqlite"), HandleCfg::default()).map_err(|e| e.to_string())?;
    let mut scored = Vec::new();
    let mut bad = Vec::new();
    for c in &cases {
        let raw = std::fs::read(base.join(&c.file)).map_err(|e| format!("{}: {}: {e}", c.id, c.file))?;
        if let Err(e) = validate(c, &String::from_utf8_lossy(&raw)) {
            bad.push(e);
            continue;
        }
        scored.push(score(&mut h, c, &raw)?);
    }
    drop(h);
    let _ = std::fs::remove_dir_all(&tmp);
    if !bad.is_empty() {
        return Err(format!("invalid cases:\n{}", bad.join("\n")));
    }
    print!("{}", report(&scored, o.rows));
    for id in &o.show {
        let s = scored.iter().find(|s| &s.case.id == id).ok_or_else(|| format!("--show {id}: no such case"))?;
        println!("== {id} {} ({}), cause lines {:?}", s.case.runner, s.level.word(), s.case.causes.iter().map(|c| c.0).collect::<Vec<_>>());
        println!("-- digest\n{}-- follow-up {}\n{}", s.texts[0], s.follow, s.texts[1]);
    }
    if let Some(g) = &o.gate {
        let earlier = std::fs::read_to_string(g).map_err(|e| format!("{}: {e}", g.display()))?;
        let r = regressions(&earlier, &scored);
        for l in &r {
            println!("regression\t{l}");
        }
        return Ok(r.is_empty());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &str = "# two cases
D001 cargo-test
file: a.txt
cmd: cargo test
exit: 101
from: a run
cause: 3 | boom
why: it went boom
middle: no

D002 generic
file: b.txt
cmd: make
exit: none
from: another run
commit: abc123
cause: 1 | warn
cause: 2 | fail
why: the warning explains the failure
middle: yes
";

    #[test]
    fn cases_parse_with_every_field() {
        let c = parse(CASES).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].exit, Some(101));
        assert_eq!(c[1].exit, None);
        assert_eq!(c[1].causes, vec![(1, "warn".to_string()), (2, "fail".to_string())]);
        assert!(c[1].middle && !c[0].middle);
        assert_eq!(c[1].commit, "abc123");
    }

    #[test]
    fn a_bad_case_file_is_refused_by_line() {
        assert!(parse("file: x\n").unwrap_err().contains("line 1"));
        assert!(parse("D1 x\nfile: a\ncmd: c\nfrom: f\nwhy: w\n").unwrap_err().contains("needs"));
        assert!(parse("D1 x\ncause: nope\n").unwrap_err().contains("LINE | TEXT"));
        assert!(parse("D1 x\nwhat: y\n").unwrap_err().contains("unknown field"));
        let two = "D1 x\nfile: a\ncmd: c\nfrom: f\ncause: 1 | t\nwhy: w\n";
        assert!(parse(&format!("{two}{two}")).unwrap_err().contains("second case"));
    }

    #[test]
    fn a_label_must_be_on_its_line() {
        let c = &parse(CASES).unwrap()[0];
        assert!(validate(c, "a\nb\nit went boom\n").is_ok());
        assert!(validate(c, "a\nboom\nc\n").unwrap_err().contains("does not hold"));
        assert!(validate(c, "a\n").unwrap_err().contains("past the end"));
    }

    #[test]
    fn levels_need_every_cause() {
        let causes = vec![(1, "warn".to_string()), (2, "fail".to_string())];
        assert_eq!(level(&causes, "warn fail", ""), Level::Digest);
        assert_eq!(level(&causes, "warn", "2\tfail"), Level::Slice);
        assert_eq!(level(&causes, "", "warn"), Level::Missed);
    }

    #[test]
    fn the_follow_up_is_chosen_from_the_digest() {
        assert_eq!(follow_up(Shape::CargoTest, "").1, "--failures");
        assert_eq!(follow_up(Shape::Summary, "").1, "--failures");
        assert_eq!(follow_up(Shape::CargoBuild, "1 errors, 0 warnings: failed\nerror:1 error[E0308] a.rs:1 x\n").1, "--item error:1");
        assert_eq!(follow_up(Shape::CargoBuild, "0 errors, 0 warnings: failed\n").1, "--tail 40");
        let g = "h:1 make exit 2, 90 lines, 2 KB\n2 lines match error|fail|panic\n45: error: no\n70: failed\nlast 3 of 90 lines:\n88: x\n";
        assert_eq!(follow_up(Shape::Generic, g).1, "--lines 35-55");
        assert_eq!(follow_up(Shape::Generic, "h:1 make, 3 lines, 9 B\n0 lines match error|fail|panic\nlast 3 of 3 lines:\n1: a\n").1, "--head 40");
    }

    #[test]
    fn the_real_store_scores_a_cargo_failure() {
        let raw = "   Compiling w v0.1.0\nwarning: unused variable: `cfg`\n running 1 test\ntest t::a ... FAILED\n\nfailures:\n\n---- t::a stdout ----\n\nthread 't::a' panicked at src/lib.rs:3:5:\nassertion failed: ok\n\nfailures:\n    t::a\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
        let tmp = std::env::temp_dir().join(format!("atlas-digest-eval-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut h = Handles::open(&tmp.join("index.sqlite"), HandleCfg::default()).unwrap();
        let mut c = parse(CASES).unwrap().remove(0);
        c.causes = vec![(11, "assertion failed: ok".into())];
        let s = score(&mut h, &c, raw.as_bytes()).unwrap();
        assert_eq!(s.level, Level::Digest, "{}", s.texts[0]);
        assert_eq!(s.follow, "--failures");
        c.causes = vec![(2, "unused variable: `cfg`".into())];
        let s = score(&mut h, &c, raw.as_bytes()).unwrap();
        // The warning is three lines from the panic: in the digest's tail.
        assert_ne!(s.level, Level::Missed, "{}", s.texts[0]);
        drop(h);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_report_counts_and_the_gate_compares() {
        let mk = |id: &str, runner: &str, lv: Level, middle: bool| Scored {
            case: Case { id: id.into(), runner: runner.into(), middle, why: "w".into(), ..Case::default() },
            level: lv,
            follow: "--failures".into(),
            shape: "cargo-test".into(),
            bytes: [400, 800, 40_000],
            texts: [String::new(), String::new()],
        };
        let s = vec![mk("D1", "cargo-test", Level::Digest, false), mk("D2", "cargo-test", Level::Missed, true), mk("D3", "go-test", Level::Slice, false)];
        let r = report(&s, true);
        assert!(r.starts_with("digest-eval: 3 cases, 2 runners; digest 1, digest+slice 2, missed 1; middle cases 1: digest 0, digest+slice 0, missed 1\n"), "{r}");
        assert!(r.contains("\ncargo-test\t2\t1\t0\t1\t200\t400\t20000\n"), "{r}");
        assert!(r.contains("\nall\t3\t1\t1\t1\t300\t600\t30000\n"), "{r}");
        assert!(r.contains("\nmiss\tD2\tcargo-test\tmiddle\tw\n"), "{r}");
        assert!(r.contains("\nrow\tD3\tgo-test\tcargo-test\tslice\t-\t--failures\t100\t200\t10000\n"), "{r}");
        let now = vec![mk("D1", "cargo-test", Level::Slice, false), mk("D3", "go-test", Level::Digest, false), mk("D9", "x", Level::Missed, false)];
        assert_eq!(regressions(&r, &now), vec!["D1: digest -> slice".to_string()]);
        // A case dropped from the corpus lowers the total: that fails too.
        let fewer = vec![mk("D1", "cargo-test", Level::Digest, false), mk("D2", "cargo-test", Level::Missed, true)];
        assert_eq!(regressions(&r, &fewer), vec!["digest+slice total: 2 -> 1".to_string()]);
    }
}
