//! `cargo xtask langs-bench --lang LANG --root DIR [--scip FILE]`: the
//! three numbers each language of atlas's language table is held to,
//! measured on a pinned public corpus (`docs/design/langs.md`, section 11).
//!
//! * **Scanner recall** (and precision): the scanner's definitions
//!   ([`atlas::code::lang_rows`]) against the SCIP index's definitions
//!   ([`atlas::code::scip_rows`]: types, callables and terms; parameters,
//!   type parameters and namespaces are not definitions there), matched by
//!   `(path, line, name)`, overall and by shape.
//! * **Item ends**: for the SCIP types and callables with an enclosing
//!   range, how often the scanner's end line equals SCIP's (and, for
//!   reference, the brace matcher's). For those without one (scip-dotnet
//!   writes none), against the end the SCIP rows store (the brace
//!   matcher's, the fallback the rows use).
//! * **SCIP rows against the grep truth**: the root indexed the way atlas
//!   indexes it (the SCIP file as the language's units, the scanner for
//!   files it lacks) into a scratch database, then the `--recall-check`
//!   code check: the share of the language's truth-regex definitions that
//!   SCIP rows hold, then split by whether the index has a document for the
//!   file: in those, held by SCIP rows, filled by the scanner (a row that
//!   fills, [`atlas::langs::Lang::fills`]) or missing; in the others, from
//!   the scanner or missing. The same with no indexer gives the scanner's
//!   share.
//!
//! Without `--scip`, the language's indexer is found as atlas finds it and
//! run at the root into a scratch folder (it may fetch modules).
//!
//! Nothing is written outside a scratch folder under the system temp
//! folder, removed at the end.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use atlas::code::{self, CodeRoot, CodeSetup, Indexers, Rows};
use atlas::code_index::{self, RustSource};
use atlas::scip;

pub const USAGE: &str = "usage: cargo xtask langs-bench --lang LANG --root DIR [--scip FILE]";

/// The pinned corpus: language, repository, commit (shallow clones of
/// public, permissively licensed repositories).
const CORPUS: &[(&str, &str, &str)] = &[
    ("go", "https://github.com/gin-gonic/gin", "0f09c3a"),
    ("typescript", "https://github.com/sindresorhus/got", "6b5aafc"),
    ("javascript", "https://github.com/GoogleCloudPlatform/microservices-demo (src/currencyservice)", "c7cb6d5"),
    ("javascript", "https://github.com/expressjs/express", "43fca52"),
    ("csharp", "https://github.com/serilog/serilog", "43105b3"),
    ("python", "https://github.com/pallets/flask", "49943b5"),
    ("java", "https://github.com/google/gson", "5773104"),
    ("kotlin", "https://github.com/square/kotlinpoet", "4ce5d54"),
];

/// Runs the bench and prints its report.
pub fn run(args: Vec<String>) -> Result<(), String> {
    let (mut lang, mut root, mut scip_file) = (None, None, None);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.next()) {
            ("--lang", Some(v)) => lang = Some(v),
            ("--root", Some(v)) => root = Some(PathBuf::from(v)),
            ("--scip", Some(v)) => scip_file = Some(PathBuf::from(v)),
            _ => return Err(USAGE.into()),
        }
    }
    let (lang, root) = (lang.ok_or(USAGE)?, root.ok_or(USAGE)?);
    let lang = match lang.as_str() {
        "ts" => "typescript".to_string(),
        "js" => "javascript".to_string(),
        "cs" => "csharp".to_string(),
        "py" => "python".to_string(),
        "kt" => "kotlin".to_string(),
        _ => lang,
    };
    let row = atlas::langs::by_name(&lang).ok_or_else(|| format!("no language row {lang:?}"))?;
    let scratch = std::env::temp_dir().join(format!("atlas-langs-bench-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let r = bench(row, &root, scip_file.as_deref(), &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    print!("{}", r?);
    Ok(())
}

fn pct(a: usize, b: usize) -> String {
    if b == 0 {
        "-".into()
    } else {
        format!("{:.1}% ({a}/{b})", a as f64 * 100.0 / b as f64)
    }
}

/// The shape of a SCIP definition's kind word: `type`, `callable`, `term`.
fn shape(kind: &str) -> &'static str {
    match kind {
        "struct" | "interface" | "type" | "class" | "enum" | "trait" | "union" => "type",
        "fn" | "method" | "ctor" => "callable",
        _ => "term",
    }
}

fn bench(row: &'static atlas::langs::Lang, root: &Path, scip_file: Option<&Path>, scratch: &Path) -> Result<String, String> {
    let mut out = String::new();
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    out.push_str(&format!("langs-bench {} at {} (commit {})\n", row.name, root.display(), head.as_deref().unwrap_or("unknown")));
    // A language may have several pinned corpora: the note is for a commit
    // that is none of them.
    let pinned: Vec<&(&str, &str, &str)> = CORPUS.iter().filter(|c| c.0 == row.name).collect();
    let on_pin = |h: &str| pinned.iter().any(|(_, _, pin)| h.starts_with(pin) || pin.starts_with(h));
    if head.as_deref().is_some_and(|h| !on_pin(h)) {
        for (l, repo, pin) in &pinned {
            out.push_str(&format!("note: a pinned {l} corpus is {repo} at {pin}; numbers differ on another commit\n"));
        }
    }
    // A language atlas never runs an indexer for (Kotlin) is measured
    // against the grep truth alone, unless a SCIP file is given.
    if scip_file.is_none() && row.unindexed.is_some() {
        return bench_scan(row, root, out);
    }
    // The SCIP file: given, or written by the language's indexer.
    let scip_path = match scip_file {
        Some(p) => p.to_path_buf(),
        None => {
            // The indexer as atlas runs it (its prerequisite, its projects),
            // through the code index itself, with the root as one unit.
            let owner = row.units_of.and_then(atlas::langs::by_name).unwrap_or(row);
            let d = owner.driver.as_ref().ok_or_else(|| format!("{} has no indexer; pass --scip", row.name))?;
            let launch = d.find()?;
            let db = scratch.join("run.sqlite");
            let spec = atlas::config::CodeSpec {
                roots: vec![atlas::config::CodeRootSpec {
                    name: "bench".into(),
                    root: root.to_path_buf(),
                    build_scripts: false,
                    scip: Default::default(),
                }],
                dir: Some(scratch.join("run")),
                timeout_s: Some(1800),
                ..Default::default()
            };
            let mut setup = CodeSetup::from_spec(&spec, &db, false);
            setup.defer_on_cargo = false;
            let mut conn = rusqlite::Connection::open(&db).map_err(|e| e.to_string())?;
            let rep = code_index::index_root(&setup, &mut conn, &setup.roots[0], &RustSource::ScanOnly("bench".into()))
                .map_err(|e| e.to_string())?;
            for (unit, r) in &rep.runs {
                out.push_str(&format!("indexer {} ({unit}) ran {:.1} s, exit {:?}\n", launch.program.display(), r.seconds, r.code));
            }
            out.push_str(&format!("units: {}\n", rep.rust));
            let p = scratch.join("run").join(format!("bench.{}.scip", d.tool));
            if !p.is_file() {
                return Err(format!("{out}no index written (units: {})", rep.rust));
            }
            p
        }
    };
    let buf = std::fs::read(&scip_path).map_err(|e| format!("{}: {e}", scip_path.display()))?;
    let ix = scip::decode(&buf, scip::Options::default()).map_err(|e| e.to_string())?;
    out.push_str(&format!("index from {} {}: {} documents\n", ix.tool.0, ix.tool.1, ix.doc_path.len()));

    let walk = code::walk(root).map_err(|e| e.to_string())?;
    let texts: Vec<(String, String)> = walk
        .source
        .iter()
        .filter(|f| atlas::langs::of_path(&f.path).is_some_and(|l| l.name == row.name))
        .filter_map(|f| code::read_source(root, &f.path).map(|t| (f.path.clone(), t)))
        .collect();
    let lines: usize = texts.iter().map(|(_, t)| t.lines().count()).sum();

    // SCIP definitions, with SCIP's own enclosing end where it gave one.
    let mut srows = Rows::default();
    let mut sfiles = HashMap::new();
    code::scip_rows(&ix, &mut srows, &mut sfiles, &|p| code::read_source(root, p));
    let mut encl: HashMap<(String, u32), u32> = HashMap::new();
    for i in 0..ix.occurrences() {
        if ix.occ_roles[i] & scip::ROLE_DEFINITION != 0 && ix.occ_encl_end[i] != scip::NONE {
            encl.insert(
                (code::rebase("", ix.doc_path[ix.occ_doc[i] as usize]), ix.occ_line[i].max(0) as u32 + 1),
                ix.occ_encl_end[i].max(0) as u32 + 1,
            );
        }
    }
    // Scored over the files the index has a document for (a file behind a
    // build tag the indexer did not load has no truth to score against).
    let docs: HashSet<String> = ix.doc_path.iter().map(|p| code::rebase("", p)).collect();
    let in_corpus: HashSet<&str> = texts.iter().map(|(p, _)| p.as_str()).filter(|p| docs.contains(*p)).collect();
    let scored: Vec<(String, String)> = texts.iter().filter(|(p, _)| in_corpus.contains(p.as_str())).cloned().collect();
    let mut outside: Vec<&String> = docs.iter().filter(|d| !texts.iter().any(|(p, _)| p == *d)).collect();
    outside.sort();
    out.push_str(&format!("index documents the walk does not list: {} {:?}\n", outside.len(), outside.iter().take(10).collect::<Vec<_>>()));
    let truth: HashMap<(String, u32, String), &str> = srows
        .occ
        .iter()
        .filter(|o| o.sect == code::SECT_DEF && in_corpus.contains(srows.files[o.file as usize].0.as_str()))
        .map(|o| ((srows.files[o.file as usize].0.clone(), o.line, o.name.clone()), shape(&o.kind)))
        .collect();

    // The scanner over the same files.
    let t0 = Instant::now();
    let mut rows = Rows::default();
    let mut files = HashMap::new();
    code::lang_rows(&texts, &mut rows, &mut files);
    let scan_ms = t0.elapsed().as_secs_f64() * 1e3;
    let mut rows = Rows::default();
    let mut files = HashMap::new();
    code::lang_rows(&scored, &mut rows, &mut files);
    let found: HashMap<(String, u32, String), u32> = rows
        .occ
        .iter()
        .map(|o| {
            ((rows.files[o.file as usize].0.clone(), o.line, o.name.clone()), rows.ends.get(&(o.file, o.line)).copied().unwrap_or(o.line))
        })
        .collect();

    let hit = truth.keys().filter(|k| found.contains_key(*k)).count();
    let right = found.keys().filter(|k| truth.contains_key(*k)).count();
    out.push_str(&format!(
        "scanner: {} files, {lines} lines in {scan_ms:.0} ms; scored on the {} files the index has\n",
        texts.len(),
        scored.len()
    ));
    out.push_str(&format!("scanner recall {}\n", pct(hit, truth.len())));
    out.push_str(&format!("scanner precision {}\n", pct(right, found.len())));
    for s in ["type", "callable", "term"] {
        let all = truth.iter().filter(|(_, v)| **v == s).count();
        let got = truth.iter().filter(|(k, v)| **v == s && found.contains_key(*k)).count();
        out.push_str(&format!("  {s}s {}\n", pct(got, all)));
    }

    // Item ends against SCIP's enclosing ranges, and where the index has
    // none, against the end the SCIP rows store.
    let (mut same, mut brace_same, mut n) = (0, 0, 0);
    let (mut rows_same, mut rows_n) = (0, 0);
    let by_path: HashMap<&str, Vec<&str>> = texts.iter().map(|(p, t)| (p.as_str(), t.lines().collect())).collect();
    let mut end_misses = Vec::new();
    for ((p, line, name), s) in &truth {
        if *s == "term" {
            continue;
        }
        let Some(&got) = found.get(&(p.clone(), *line, name.clone())) else { continue };
        let Some(&want) = encl.get(&(p.clone(), *line)) else {
            let brace = by_path.get(p.as_str()).map_or(*line, |l| atlas::digest::item_end(l, *line as usize - 1) as u32).max(*line);
            rows_n += 1;
            if got == brace {
                rows_same += 1;
            } else {
                end_misses.push(format!("{p}:{line} {name}: scanner {got}, brace matcher {brace}"));
            }
            continue;
        };
        n += 1;
        if got == want {
            same += 1;
        } else {
            end_misses.push(format!("{p}:{line} {name}: scanner {got}, SCIP {want}"));
        }
        if let Some(l) = by_path.get(p.as_str()) {
            if atlas::digest::item_end(l, *line as usize - 1) as u32 == want {
                brace_same += 1;
            }
        }
    }
    out.push_str(&format!(
        "item ends equal to SCIP's (types and callables): scanner {}, brace matcher {}\n",
        pct(same, n),
        pct(brace_same, n)
    ));
    if rows_n > 0 {
        out.push_str(&format!(
            "item ends equal to the brace matcher's where the index has no enclosing range (types and callables): scanner {}\n",
            pct(rows_same, rows_n)
        ));
    }

    // The atlas pipeline: SCIP rows, then the scanner alone, each against
    // the grep truth of the recall check.
    for (label, rust, indexers) in [
        ("SCIP rows", RustSource::File(scip_path.clone()), Indexers::Off("bench".into())),
        ("scanner alone", RustSource::ScanOnly("bench".into()), Indexers::Off("bench".into())),
    ] {
        let db = scratch.join(format!("{}.sqlite", label.replace(' ', "-")));
        let spec = atlas::config::CodeSpec {
            roots: vec![atlas::config::CodeRootSpec {
                name: "bench".into(),
                root: root.to_path_buf(),
                build_scripts: false,
                scip: Default::default(),
            }],
            dir: Some(scratch.join("code")),
            ..Default::default()
        };
        let mut setup = CodeSetup::from_spec(&spec, &db, false);
        setup.indexers = indexers;
        setup.defer_on_cargo = false;
        let mut conn = rusqlite::Connection::open(&db).map_err(|e| e.to_string())?;
        let rep = code_index::index_root(
            &setup,
            &mut conn,
            &CodeRoot { name: "bench".into(), root: root.to_path_buf(), build_scripts: false, scip: Default::default() },
            &rust,
        )
        .map_err(|e| e.to_string())?;
        let c = atlas::recall::code_check(&conn, &setup.roots).map_err(|e| e.to_string())?;
        let l = c.langs.iter().find(|l| l.lang == row.name).cloned().unwrap_or_default();
        out.push_str(&format!(
            "{label} against the grep truth: {} indexed ({} of the definitions in files the index has), {} scanner only, {} missing of {} ({} in comments); units: {}\n",
            pct(l.indexed, l.truth),
            pct(l.indexed, l.truth - l.scan_only - l.missing),
            l.scan_only,
            l.missing,
            l.truth,
            l.excluded,
            rep.rust
        ));
        if label == "SCIP rows" {
            out.push_str(&truth_split(&conn, row, root, &docs)?);
        }
        for m in c.misses.iter().filter(|m| m.lang == row.name).take(10) {
            out.push_str(&format!("  miss {} {} at {}:{}\n", m.keyword, m.name, m.path, m.line));
        }
    }

    // What the scanner misses and adds, for tuning the row.
    // How many of each list to print (`LANGS_BENCH_SHOW`, default 25).
    let show: usize = std::env::var("LANGS_BENCH_SHOW").ok().and_then(|s| s.parse().ok()).unwrap_or(25);
    let mut misses: Vec<String> =
        truth.iter().filter(|(k, _)| !found.contains_key(*k)).map(|((p, l, n), s)| format!("{s} {n} at {p}:{l}")).collect();
    misses.sort();
    let mut extra: Vec<String> = found.keys().filter(|k| !truth.contains_key(*k)).map(|(p, l, n)| format!("{n} at {p}:{l}")).collect();
    extra.sort();
    end_misses.sort();
    for (title, list) in [("scanner misses", &misses), ("scanner extras", &extra), ("end differences", &end_misses)] {
        out.push_str(&format!("{title}: {}\n", list.len()));
        for m in list.iter().take(show) {
            out.push_str(&format!("  {m}\n"));
        }
    }
    Ok(out)
}

/// The grep truth of the recall check, split by whether the index has a
/// document for the file: in those, how many SCIP rows hold, how many the
/// scanner filled and how many are missing; in the others, how many come
/// from the scanner and how many are missing.
fn truth_split(
    conn: &rusqlite::Connection,
    row: &'static atlas::langs::Lang,
    root: &Path,
    docs: &HashSet<String>,
) -> Result<String, String> {
    // (path, line, name) -> only scanner rows?
    let mut have: HashMap<(String, u32, String), bool> = HashMap::new();
    let mut st = conn
        .prepare("SELECT f.path, o.line, o.name, o.kind FROM code_occ o JOIN code_files f ON f.id = o.file WHERE f.root = 'bench' AND o.sect = 0")
        .map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
        .map_err(|e| e.to_string())?;
    for r in rows {
        let (p, l, n, k) = r.map_err(|e| e.to_string())?;
        let scan = k.ends_with("(scan)");
        let e = have.entry((p, l, n)).or_insert(scan);
        *e = *e && scan;
    }
    let re = atlas::regex::Regex::new(row.truth).map_err(|e| e.to_string())?;
    let ctors = atlas::recall::ctor_rows(conn, "bench").map_err(|e| e.to_string())?;
    let scanner = atlas::scanner::of(row);
    let walk = code::walk(root).map_err(|e| e.to_string())?;
    // [in documents, elsewhere] x [SCIP rows, scanner, missing]
    let mut n = [[0usize; 3]; 2];
    let mut missing_in: Vec<String> = Vec::new();
    for f in walk.source.iter().filter(|f| atlas::langs::of_path(&f.path).is_some_and(|l| l.name == row.name)) {
        let Some(text) = code::read_source(root, &f.path) else { continue };
        let code_lines = scanner.code_lines(&text);
        let side = usize::from(!docs.contains(&f.path));
        for (i, line) in text.lines().enumerate() {
            let Some(c) = re.captures(line) else { continue };
            let name = c.name("name").map_or("", |m| m.as_str());
            if name == "_" || !code_lines.get(i).copied().unwrap_or(true) {
                continue;
            }
            let held = have
                .get(&(f.path.clone(), i as u32 + 1, name.to_string()))
                .or_else(|| code::CTORS.contains(&name).then(|| ctors.get(&(f.path.clone(), i as u32 + 1))).flatten());
            let k = match held {
                Some(false) => 0,
                Some(true) => 1,
                None => 2,
            };
            n[side][k] += 1;
            if side == 0 && k == 2 {
                missing_in.push(format!("{name} at {}:{}", f.path, i + 1));
            }
        }
    }
    let in_docs: usize = n[0].iter().sum();
    let mut out = format!(
        "  in the files the index has: {} by SCIP rows, {} filled by the scanner, {} missing of {in_docs}; in the files it lacks: {} from the scanner, {} missing of {}\n",
        pct(n[0][0], in_docs),
        n[0][1],
        n[0][2],
        n[1][1] + n[1][0],
        n[1][2],
        n[1].iter().sum::<usize>()
    );
    for m in missing_in.iter().take(10) {
        out.push_str(&format!("  missing in an indexed file: {m}\n"));
    }
    Ok(out)
}

/// The bench of a language without an indexer (Kotlin): the scanner
/// against the recall check's truth regex, and item ends against the brace
/// matcher.
///
/// * Recall: the truth's definitions (`kw`, `name` on a code line) the
///   scanner defines at that line with that name.
/// * Precision: of the scanner's definitions on lines the truth counts,
///   the share that name the truth's name. Definitions on other lines
///   (members without a modifier, enum entries, constructor properties:
///   lines the truth leaves out because a local could look the same) are
///   counted by kind and listed, for reading.
/// * Item ends: for the scanner's types and callables whose line opens a
///   brace body (the brace matcher finds one closing), the share whose end
///   is the brace matcher's.
fn bench_scan(row: &'static atlas::langs::Lang, root: &Path, mut out: String) -> Result<String, String> {
    let walk = code::walk(root).map_err(|e| e.to_string())?;
    let texts: Vec<(String, String)> = walk
        .source
        .iter()
        .filter(|f| atlas::langs::of_path(&f.path).is_some_and(|l| l.name == row.name))
        .filter_map(|f| code::read_source(root, &f.path).map(|t| (f.path.clone(), t)))
        .collect();
    let lines: usize = texts.iter().map(|(_, t)| t.lines().count()).sum();
    let re = atlas::regex::Regex::new(row.truth).map_err(|e| e.to_string())?;
    let scanner = atlas::scanner::of(row);
    let t0 = Instant::now();
    let defs: Vec<(String, Vec<atlas::scanner::Def>)> = texts.iter().map(|(p, t)| (p.clone(), scanner.scan_file(p, t))).collect();
    let scan_ms = t0.elapsed().as_secs_f64() * 1e3;
    out.push_str(&format!("no indexer: the scanner against the grep truth; {} files, {lines} lines in {scan_ms:.0} ms\n", texts.len()));
    // (path, line) -> the truth's name.
    let mut truth: HashMap<(String, u32), String> = HashMap::new();
    for (p, t) in &texts {
        let code_lines = scanner.code_lines(t);
        for (i, line) in t.lines().enumerate() {
            let Some(c) = re.captures(line) else { continue };
            let name = c.name("name").map_or("", |m| m.as_str());
            if name == "_" || !code_lines.get(i).copied().unwrap_or(true) {
                continue;
            }
            truth.insert((p.clone(), i as u32 + 1), name.to_string());
        }
    }
    let found: HashSet<(String, u32, String)> =
        defs.iter().flat_map(|(p, ds)| ds.iter().map(move |d| (p.clone(), d.line, d.name.clone()))).collect();
    let mut misses: Vec<String> = Vec::new();
    let mut hit = 0;
    for ((p, l), n) in &truth {
        if found.contains(&(p.clone(), *l, n.clone())) {
            hit += 1;
        } else {
            misses.push(format!("{n} at {p}:{l}"));
        }
    }
    out.push_str(&format!("scanner recall {}\n", pct(hit, truth.len())));
    // Per line the truth counts: right when one of the line's definitions
    // has the truth's name (the others on it, a constructor's properties
    // on a class's line, count as definitions the truth does not see).
    let (mut on, mut right) = (0, 0);
    let mut wrong: Vec<String> = Vec::new();
    let mut other: HashMap<&str, usize> = HashMap::new();
    let mut other_list: Vec<String> = Vec::new();
    for (p, ds) in &defs {
        let mut lines_seen: HashSet<u32> = HashSet::new();
        for d in ds {
            let t = truth.get(&(p.clone(), d.line));
            let named = t.is_some_and(|n| ds.iter().any(|e| e.line == d.line && e.name == *n));
            match t {
                Some(n) if lines_seen.insert(d.line) => {
                    on += 1;
                    if named {
                        right += 1;
                    } else {
                        wrong.push(format!("{} {} at {p}:{} (truth: {n})", d.kind, d.name, d.line));
                    }
                }
                Some(n) if *n == d.name => {}
                _ => {
                    *other.entry(d.kind).or_default() += 1;
                    other_list.push(format!("{} {}.{} at {p}:{}", d.kind, d.container, d.name, d.line));
                }
            }
        }
    }
    out.push_str(&format!("scanner precision on the truth's lines {}\n", pct(right, on)));
    let mut kinds: Vec<(&&str, &usize)> = other.iter().collect();
    kinds.sort();
    out.push_str(&format!(
        "scanner definitions on lines the truth does not count: {} ({})\n",
        other.values().sum::<usize>(),
        kinds.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ")
    ));
    // Item ends against the brace matcher, where it finds a brace body
    // that opens before the next definition (Kotlin's expression bodies and
    // bodiless classes have none: the matcher would run on to a later
    // item's braces).
    let (mut same, mut n, mut bodiless) = (0, 0, 0);
    let mut end_misses: Vec<String> = Vec::new();
    for ((p, ds), (_, t)) in defs.iter().zip(&texts) {
        let ls: Vec<&str> = t.lines().collect();
        let (clean, _) = scanner.blank(t);
        let cl: Vec<&str> = clean.lines().collect();
        for d in ds.iter().filter(|d| shape(d.kind) != "term" || d.kind == "object") {
            let next = ds.iter().map(|e| e.line).filter(|l| *l > d.line).min().unwrap_or(u32::MAX);
            let opens = (d.line as usize - 1..cl.len()).find(|&i| cl[i].contains('{')).map(|i| i as u32 + 1);
            if opens.is_none_or(|o| o >= next) {
                bodiless += 1;
                continue;
            }
            let Some(brace) = atlas::digest::brace_body_end(&ls, d.line as usize - 1) else { continue };
            n += 1;
            if brace as u32 == d.end {
                same += 1;
            } else {
                end_misses.push(format!("{p}:{} {}: scanner {}, brace matcher {brace}", d.line, d.name, d.end));
            }
        }
    }
    out.push_str(&format!(
        "item ends equal to the brace matcher's (types and callables with a brace body before the next definition): scanner {}; {bodiless} without one\n",
        pct(same, n)
    ));
    let show: usize = std::env::var("LANGS_BENCH_SHOW").ok().and_then(|s| s.parse().ok()).unwrap_or(25);
    misses.sort();
    wrong.sort();
    other_list.sort();
    end_misses.sort();
    for (title, list) in
        [("scanner misses", &misses), ("wrong names", &wrong), ("definitions outside the truth", &other_list), ("end differences", &end_misses)]
    {
        out.push_str(&format!("{title}: {}\n", list.len()));
        for m in list.iter().take(show) {
            out.push_str(&format!("  {m}\n"));
        }
    }
    Ok(out)
}
