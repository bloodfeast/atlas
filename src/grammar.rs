//! The whole-corpus recall test: every id-shaped token in a defining
//! position must index as a definition. Unmatched shapes are reported with
//! a suggested pattern, never adopted automatically.
//!
//! The scan is pattern-free. An id shape is `[A-Z][A-Z0-9]*(-[A-Z0-9]+)+`
//! with an optional lowercase letter at the end (`TF-12`, `TASK-28n`,
//! `ATLAS-LEAN`). Defining positions are the registry's own
//! ([`crate::registry::extract`]): a frontmatter id key, the file stem, the
//! first identifier on a heading, the first cell of a table row, and a list
//! item introducing it (a leading ISO date or time counts as markup).
//!
//! Each hit is checked against the index (`idents`, rank above 0):
//!
//! * indexed as a definition: covered;
//! * not indexed, and a configured pattern matches it: a **failure**
//!   (`atlas --recall-check` exits 1);
//! * not indexed, and no pattern matches it: **unmatched**. Unmatched hits
//!   are grouped by generalised shape (the prefix kept, digits as `\d+`, a
//!   letter suffix as `[a-z]?`), and each group gets a suggested pattern
//!   with its kind (the lowercased prefix), its count and three samples.
//!
//! Surfacing: `atlas --recall-check` fails on failures; the server's first
//! index logs each failure to the misses table (tool `grammar`), once per
//! configuration and never twice for one failure. Suggestions are not
//! misses: they are listed on stderr and by `atlas grammar list`, and a
//! suggestion an older version filed as a miss is resolved. `atlas grammar
//! list | accept N | reject N` shows them, writes an accepted pattern into the configuration's
//! `ids` (diffed and backed up, undone by `atlas settings undo`) or records
//! a rejection. Nothing is adopted automatically.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use crate::error::Result;
use crate::json::{Map, Value};
use crate::registry::{self, DocInfo, Names, PatternSpec, Rules};
use crate::store::Store;

/// The id shape, as a registry pattern (word-bounded).
pub const SHAPE: &str = r"\b[A-Z][A-Z0-9]*(?:-[A-Z0-9]+)+[a-z]?\b";

/// The marker of the last first-index check, beside the database.
pub const CHECK_STATE: &str = "grammar-check.json";
/// Rejected suggestions, beside the database.
pub const REJECTED: &str = "grammar-rejected.json";

/// One suggested pattern for a group of unmatched hits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The kind it would get (the lowercased prefix).
    pub kind: String,
    /// The regex.
    pub regex: String,
    /// Defining hits of the shape.
    pub count: usize,
    /// Up to three `source/path:line` samples.
    pub samples: Vec<String>,
    /// Distinct identifiers of the shape.
    pub ids: usize,
}

crate::impl_to_json!(Suggestion { kind, regex, count, samples, ids });

/// What the scan found.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Documents scanned.
    pub docs: usize,
    /// Id-shaped tokens in a defining position.
    pub hits: usize,
    /// Of those, indexed as definitions.
    pub covered: usize,
    /// Hits a configured pattern matches that are not indexed as
    /// definitions, one line each.
    pub failures: Vec<String>,
    /// Groups of hits no configured pattern matches, most hits first.
    pub suggestions: Vec<Suggestion>,
    /// Scan time.
    pub ms: u128,
}

/// The generalised shape of a token: its group key and whether it ends in
/// a lowercase letter.
pub fn shape_key(token: &str) -> (String, bool) {
    let mut suffix = false;
    let mut parts = Vec::new();
    for (i, seg) in token.split('-').enumerate() {
        if i == 0 {
            parts.push(seg.to_string());
            continue;
        }
        let (body, last) = match seg.char_indices().last() {
            Some((at, c)) if c.is_ascii_lowercase() => (&seg[..at], true),
            _ => (seg, false),
        };
        let letters = seg.bytes().take_while(u8::is_ascii_uppercase).count();
        if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()) {
            parts.push(r"\d+".to_string());
            suffix |= last;
        } else if seg.len() == 1 && letters == 1 {
            // A one-letter variant (`LEVERS-A`, `LEVERS-B`).
            parts.push("[A-Z]".to_string());
        } else if letters > 0 && letters < seg.len() && seg[letters..].bytes().all(|b| b.is_ascii_digit()) {
            // Letters then digits (`Q-M1`, `Q-A1`).
            parts.push(r"[A-Z]+\d+".to_string());
        } else {
            parts.push(seg.to_string());
        }
    }
    (parts.join("-"), suffix)
}

fn kind_for(prefix: &str, taken: &HashSet<String>) -> String {
    let mut k: String = prefix.to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(28).collect();
    if k.is_empty() || k == registry::NOTE_KIND || k == registry::FRONTMATTER_KIND {
        k = format!("id-{k}");
    }
    let base = k.clone();
    let mut n = 2;
    while taken.contains(&k) {
        k = format!("{base}-{n}");
        n += 1;
    }
    k
}

fn read_set(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| crate::json::parse(&t).ok())
        .and_then(|v| v.get("rejected").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()))
        .unwrap_or_default()
}

/// Scans every indexed document. Suggestions whose regex is in `rejected`
/// are left out.
///
/// # Errors
///
/// [`crate::Error::Db`] from SQLite.
pub fn scan(store: &Store, rejected: &HashSet<String>) -> Result<Report> {
    let t0 = Instant::now();
    let shape = Rules::new(&[PatternSpec { name: "shape".into(), regex: SHAPE.into(), case_insensitive: false, exclude: Vec::new() }], false)?;
    let configured = store.rules();
    let names = Names::default();
    let id_keys = vec!["id".to_string()];
    let mut report = Report::default();
    // key -> (suffix, count, distinct ids, samples)
    let mut groups: BTreeMap<String, (bool, usize, HashSet<String>, Vec<String>)> = BTreeMap::new();
    let mut defs_st = store.conn().prepare("SELECT lower(i.ident), i.line FROM idents i JOIN docs d ON d.id = i.doc_id WHERE d.source = ?1 AND d.path = ?2 AND i.rank > 0")?;
    for (source, path) in store.docs()? {
        let Ok(bytes) = store.read_document(&source, &path) else { continue };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        report.docs += 1;
        let stem = Path::new(&path).file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let info = DocInfo { stem, id_keys: &id_keys, names_kind: None };
        let occs = registry::extract(&shape, &names, &text, &[], &info);
        let mut defined: Option<HashSet<(String, i64)>> = None;
        let lines: Vec<&str> = text.lines().collect();
        for o in occs.iter().filter(|o| o.def.is_some()) {
            report.hits += 1;
            let defined = match &mut defined {
                Some(d) => d,
                None => {
                    let rows = defs_st.query_map([&source, &path], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                    defined.insert(rows.collect::<std::result::Result<_, _>>()?)
                }
            };
            let lower = o.ident.to_lowercase();
            if defined.contains(&(lower.clone(), o.line as i64)) {
                report.covered += 1;
                continue;
            }
            let at = format!("{source}/{path}:{}", o.line);
            // Does a configured pattern match this very token on its line
            // (or in the stem, for a file-level definition)?
            let line = lines.get(o.line.saturating_sub(1)).map(|l| l.trim_end_matches('\r')).unwrap_or("");
            let mut m = Vec::new();
            configured.matches(line, &mut m);
            let first = m.iter().min_by_key(|x| x.0).map(|x| x.2);
            configured.matches(stem, &mut m);
            if let Some(&(_, _, _, k)) = m.iter().find(|x| x.2.eq_ignore_ascii_case(&o.ident)) {
                // On a heading, row or list item the index defines the first
                // configured identifier; when that is another one, the index
                // chose deliberately and this is not a failure.
                let positional = !matches!(o.def, Some(registry::Why::File | registry::Why::Frontmatter | registry::Why::Field));
                if positional && first.is_some_and(|f| !f.eq_ignore_ascii_case(&o.ident)) {
                    report.covered += 1;
                    continue;
                }
                report.failures.push(format!(
                    "{} at {at}: pattern {} matches it in a defining position ({}), but the index has no definition there",
                    o.ident,
                    configured.patterns[k].name,
                    o.def.map(|d| d.as_str()).unwrap_or("")
                ));
                continue;
            }
            let (key, suffix) = shape_key(&o.ident);
            let g = groups.entry(key).or_insert_with(|| (false, 0, HashSet::new(), Vec::new()));
            g.0 |= suffix;
            g.1 += 1;
            g.2.insert(lower);
            if g.3.len() < 3 {
                g.3.push(at);
            }
        }
    }
    let mut taken: HashSet<String> = configured.patterns.iter().map(|p| p.name.clone()).collect();
    let mut sugg: Vec<Suggestion> = Vec::new();
    for (key, (suffix, count, ids, samples)) in groups {
        let regex = format!(r"\b{key}{}\b", if suffix { "[a-z]?" } else { "" });
        if rejected.contains(&regex) {
            continue;
        }
        let prefix = key.split('-').next().unwrap_or("");
        let kind = kind_for(prefix, &taken);
        taken.insert(kind.clone());
        sugg.push(Suggestion { kind, regex, count, samples, ids: ids.len() });
    }
    sugg.sort_by(|a, b| b.count.cmp(&a.count).then(a.regex.cmp(&b.regex)));
    report.suggestions = sugg;
    report.ms = t0.elapsed().as_millis();
    Ok(report)
}

/// The whole-corpus part of `atlas --recall-check`: one line per failure.
///
/// # Errors
///
/// [`crate::Error::Db`] from SQLite.
pub fn recall_check(store: &Store) -> Result<Vec<String>> {
    Ok(scan(store, &HashSet::new())?.failures)
}

/// The configuration's fingerprint for the first-index check: the patterns
/// and the sources. A change runs the check again.
fn fingerprint(store: &Store) -> String {
    let mut s = String::new();
    for p in &store.rules().patterns {
        s.push_str(&p.name);
        s.push('\u{1}');
        s.push_str(p.re.as_str());
        s.push('\u{2}');
    }
    for src in store.sources() {
        s.push_str(&src.name);
        s.push('\u{3}');
    }
    for v in store.vaults().all() {
        s.push_str(&v.name);
        s.push('\u{4}');
    }
    crate::sha256::hex(s.as_bytes())
}

/// The first-index check, run once per configuration: brings the index up
/// to date, scans, and logs each failure not already open to the misses
/// table (tool `grammar`). Suggestions are not misses; an open `grammar`
/// miss that is not a failure line (a suggestion an older version filed) is
/// resolved. Returns the report, or `None` when this configuration
/// was already checked.
///
/// # Errors
///
/// [`crate::Error::Db`] or [`crate::Error::Io`].
pub fn first_index_check(store: &mut Store, db: &Path) -> Result<Option<Report>> {
    let dir = db.parent().unwrap_or(Path::new("."));
    let marker = dir.join(CHECK_STATE);
    let fp = fingerprint(store);
    let done = std::fs::read_to_string(&marker).ok().and_then(|t| crate::json::parse(&t).ok()).and_then(|v| v.get("fingerprint").and_then(Value::as_str).map(str::to_string));
    if done.as_deref() == Some(fp.as_str()) {
        return Ok(None);
    }
    store.rescan()?;
    store.rescan_sources(None)?;
    let report = scan(store, &read_set(&dir.join(REJECTED)))?;
    let (open, _) = store.misses(Some("open"), 100_000)?;
    let logged: HashSet<String> = open.iter().filter(|m| m.tool == "grammar").map(|m| m.query.clone()).collect();
    for f in &report.failures {
        if !logged.contains(f) {
            store.report_miss("grammar", f, "no definition in the index", "", "a configured pattern matches an identifier in a defining position that is not indexed as a definition", "atlas")?;
        }
    }
    for m in open.iter().filter(|m| m.tool == "grammar" && m.returned.is_empty() && !m.query.contains(" at ")) {
        store.resolve_miss(m.id, "a suggested pattern, not a miss: see `atlas grammar list`", "atlas")?;
    }
    let v = Value::Object([("fingerprint".to_string(), Value::String(fp)), ("time".to_string(), Value::String(crate::memory::now()))].into_iter().collect());
    crate::fsio::replace_atomic(&marker, (crate::json::to_string_pretty(&v) + "\n").as_bytes())?;
    Ok(Some(report))
}

/// Runs [`first_index_check`] on its own thread and store connection, so
/// the server's first answer is not delayed.
pub fn spawn_first_index_check(store: Store, db: PathBuf) {
    let spawned = std::thread::Builder::new().name("atlas-grammar".into()).spawn(move || {
        let mut store = store;
        match first_index_check(&mut store, &db) {
            Ok(Some(r)) if !r.failures.is_empty() || !r.suggestions.is_empty() => eprintln!(
                "atlas grammar: {} failure(s), {} suggested pattern(s) in {} ms; see `atlas grammar list`",
                r.failures.len(),
                r.suggestions.len(),
                r.ms
            ),
            Ok(_) => {}
            Err(e) => eprintln!("atlas grammar: {e}"),
        }
    });
    if let Err(e) = spawned {
        eprintln!("atlas grammar: {e}");
    }
}

fn print_report(r: &Report) {
    println!(
        "grammar: {} documents, {} id-shaped tokens in defining positions, {} indexed as definitions, {} failure(s), {} unmatched shape(s) ({} ms)",
        r.docs,
        r.hits,
        r.covered,
        r.failures.len(),
        r.suggestions.len(),
        r.ms
    );
    for f in &r.failures {
        println!("FAIL {f}");
    }
    for (i, s) in r.suggestions.iter().enumerate() {
        println!("{:>3}. kind {:?} regex {:?}: {} hit(s), {} id(s); e.g. {}", i + 1, s.kind, s.regex, s.count, s.ids, s.samples.join(", "));
    }
    if !r.suggestions.is_empty() {
        println!("accept one with `atlas grammar accept N` (writes it to the configuration's ids, with a backup), or `atlas grammar reject N`");
    }
}

fn resolve_grammar_misses(store: &mut Store, query: &str, note: &str) -> Result<()> {
    let (open, _) = store.misses(Some("open"), 100_000)?;
    for m in open.iter().filter(|m| m.tool == "grammar" && m.query == query) {
        store.resolve_miss(m.id, note, "atlas")?;
    }
    Ok(())
}

/// The `atlas grammar` subcommand (list, accept, reject).
///
/// # Arguments
///
/// * `args` - the words after `grammar`.
/// * `store` - the opened index.
/// * `config` - the configuration file `accept` writes to.
/// * `db` - the index database (the rejections are kept beside it).
///
/// # Errors
///
/// A message for the user when the subcommand cannot run.
pub fn cli(args: Vec<String>, store: &mut Store, config: Option<&Path>, db: &Path) -> std::result::Result<ExitCode, String> {
    let e = |x: crate::Error| x.to_string();
    let dir = db.parent().unwrap_or(Path::new(".")).to_path_buf();
    let rejected_path = dir.join(REJECTED);
    let mut rejected = read_set(&rejected_path);
    store.rescan().map_err(e)?;
    store.rescan_sources(None).map_err(e)?;
    let report = scan(store, &rejected).map_err(e)?;
    let word = args.first().map(String::as_str).unwrap_or("list");
    let pick = || -> std::result::Result<&Suggestion, String> {
        let n: usize = args.get(1).and_then(|n| n.parse().ok()).ok_or(format!("{word} needs N, a number from `atlas grammar list`"))?;
        report.suggestions.get(n.wrapping_sub(1)).ok_or(format!("no suggestion {n}; `atlas grammar list` shows {}", report.suggestions.len()))
    };
    match word {
        "list" => {
            print_report(&report);
            Ok(if report.failures.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "accept" => {
            let s = pick()?.clone();
            let config = config.ok_or("accept writes to the configuration file, and none is in use (--vault flags only)")?;
            let text = std::fs::read_to_string(config).map_err(|x| format!("{}: {x}", config.display()))?;
            let cur = crate::json::parse(&text).map_err(|x| format!("{}: {}", config.display(), x.msg))?;
            let mut ids: Vec<Value> = cur.get("ids").and_then(Value::as_array).cloned().unwrap_or_default();
            let spec: Map = [("name".to_string(), Value::String(s.kind.clone())), ("regex".to_string(), Value::String(s.regex.clone()))].into_iter().collect();
            ids.push(Value::Object(spec));
            // The whole list must still compile (names unique, regexes valid).
            let specs: Vec<PatternSpec> = ids
                .iter()
                .map(|v| PatternSpec {
                    name: v.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    regex: v.get("regex").and_then(Value::as_str).unwrap_or("").to_string(),
                    case_insensitive: v.get("case_insensitive").and_then(Value::as_bool).unwrap_or(false),
                    exclude: Vec::new(),
                })
                .collect();
            Rules::new(&specs, false).map_err(e)?;
            let p = crate::claude_settings::Paths::detect(Some(config), db).ok_or("no home folder is known for the backup (set HOME or USERPROFILE)")?;
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let mut out = std::io::stdout();
            let mut io = crate::claude_settings::Io { out: &mut out, input: &mut input, interactive: false, version: None, dry_run: false };
            let label = format!("accepted: kind {:?} regex {:?}", s.kind, s.regex);
            let manual = format!("append {{\"name\": {}, \"regex\": {}}} to \"ids\"", Value::String(s.kind.clone()), Value::String(s.regex.clone()));
            let ok = crate::claude_settings::edit_config(&p, &mut io, &["ids"], Some(&Value::Array(ids)), &label, &manual)?;
            if ok {
                resolve_grammar_misses(store, &s.regex, "accepted into the configuration's ids").map_err(e)?;
                println!("restart Claude Code (or reconnect atlas with /mcp) to index with it");
            }
            Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        "reject" => {
            let s = pick()?.clone();
            rejected.insert(s.regex.clone());
            let mut list: Vec<String> = rejected.into_iter().collect();
            list.sort();
            let v = Value::Object([("rejected".to_string(), Value::Array(list.into_iter().map(Value::String).collect()))].into_iter().collect());
            crate::fsio::replace_atomic(&rejected_path, (crate::json::to_string_pretty(&v) + "\n").as_bytes()).map_err(e)?;
            resolve_grammar_misses(store, &s.regex, "rejected").map_err(e)?;
            println!("rejected {:?}: it is not suggested again ({})", s.regex, rejected_path.display());
            Ok(ExitCode::SUCCESS)
        }
        w => Err(format!("unknown grammar command {w:?}\nusage: atlas grammar list | accept N | reject N")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generalises_shapes() {
        assert_eq!(shape_key("TF-12"), (r"TF-\d+".to_string(), false));
        assert_eq!(shape_key("TASK-28n"), (r"TASK-\d+".to_string(), true));
        assert_eq!(shape_key("ATLAS-LEAN"), ("ATLAS-LEAN".to_string(), false));
        assert_eq!(shape_key("X1-Y2-7"), (r"X1-[A-Z]+\d+-\d+".to_string(), false));
        assert_eq!(shape_key("LEVERS-B"), ("LEVERS-[A-Z]".to_string(), false));
        assert_eq!(shape_key("Q-M1"), (r"Q-[A-Z]+\d+".to_string(), false));
        assert_eq!(shape_key("GPU-IDLE-4K"), ("GPU-IDLE-4K".to_string(), false));
    }

    #[test]
    fn kinds_avoid_taken_and_reserved_names() {
        let taken: HashSet<String> = ["task".to_string()].into_iter().collect();
        assert_eq!(kind_for("TASK", &taken), "task-2");
        assert_eq!(kind_for("TF", &taken), "tf");
        assert_eq!(kind_for("NOTE", &taken), "id-note");
    }

    #[test]
    fn the_shape_finds_ids_and_skips_dates() {
        let r = Rules::new(&[PatternSpec { name: "s".into(), regex: SHAPE.into(), case_insensitive: false, exclude: Vec::new() }], false).unwrap();
        let mut m = Vec::new();
        r.matches("2026-10-31 TF-12, TASK-28n and ATLAS-LEAN; not Ab-Cd or task-3", &mut m);
        let got: Vec<&str> = m.iter().map(|x| x.2).collect();
        assert_eq!(got, ["TF-12", "TASK-28n", "ATLAS-LEAN"]);
    }
}
