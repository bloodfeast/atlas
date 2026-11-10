//! The write-site ratchet: every raw SQLite write call in the non-test part
//! of `src/` is counted per file against the table below, and each file's
//! row says how its writes are guarded (or why they need no guard).
//!
//! A text scan cannot tell a guarded write from a raw one (most guarded
//! writes are textually raw inside a closure), so this is a ratchet, not a
//! proof: a new write site changes a count and fails the test until someone
//! checks it and updates the row.
//!
//! The rule the rows follow: every write to the memory store or the index
//! store goes through `memory::guarded`, `memory::check_locked`,
//! `store::guarded`, `store::guard`, `store::begin` or `store::begin_shared`,
//! so once a newer atlas has upgraded a store, an older one that opened it
//! earlier stops writing. Never rely on `guarded` to check a write that runs
//! inside a transaction someone else opened (it runs inline there, with no
//! check): check where the transaction opens. A `CREATE TABLE IF NOT EXISTS`
//! or `ALTER` that directly precedes a write goes inside its guarded
//! transaction; the setup DDL left outside is listed with its reason.

use std::collections::BTreeMap;
use std::path::Path;

/// What counts as a raw write call.
const PATTERNS: &[&str] = &[".execute(", "execute_batch(", "transaction(", "Transaction::new_unchecked", "\"BEGIN", "transaction_with_behavior("];

/// (file under `src/`, raw write calls, how they are guarded).
const SITES: &[(&str, usize, &str)] = &[
    ("baseline.rs", 4, "ensure: CREATE and ALTER under store::guarded after a read-only look; save under store::guarded; AtlasSide::read TOKEN_TABLE is read-path setup DDL"),
    ("calibrate/mod.rs", 7, "run: tables under store::guarded, each batch under store::begin_shared, the cutoff row under store::guarded"),
    ("code.rs", 31, "ensure_schema re-checks under the lock (its meta CREATE is setup DDL before it); write_root and reindex_file under store::begin; mark_package_users under store::guarded"),
    ("code_index.rs", 3, "code_roots reset under store::guarded; take_lease under store::begin; release_lease under store::guarded"),
    ("compact.rs", 4, "log: table and row under store::guarded; totals TABLE DDL is read-path setup DDL"),
    ("handles.rs", 5, "open: HANDLES_SCHEMA after open_raw's check (setup DDL); put, touch and evict under store::guarded"),
    ("hook_hits.rs", 2, "log_injection under store::guarded; logged TOKEN_TABLE is read-path setup DDL"),
    ("hook_rewrite.rs", 3, "TOKEN_TABLE after Handles::open (setup DDL); both miss rows under store::guarded"),
    ("hooks.rs", 1, "TOKEN_TABLE on the token-log thread after open_raw's check (setup DDL)"),
    ("memory/archive.rs", 8, "archive.sqlite, a separate file: out of scope"),
    ("memory/check.rs", 3, "a read snapshot transaction; a lock taken only to read; VACUUM INTO writes a copy, not the store"),
    ("memory/export.rs", 2, "write: its own BEGIN IMMEDIATE (or the caller's) and memory::check_locked before any file; the export_files row inside it"),
    ("memory/mod.rs", 31, "open and migrate path (make_schema re-checks under the lock); guarded and check_locked themselves; Writer opens with check_locked; meta_set and record_jsonl_end under memory::guarded"),
    ("memory/vectors.rs", 4, "ensure_schema is read-path setup DDL (prune runs it under memory::guarded); embed_pending under memory::check_locked; prune under memory::guarded"),
    ("pr_digest.rs", 7, "record: BEGIN IMMEDIATE then store::guard, COMMIT or ROLLBACK; pr_heads under store::guarded"),
    ("redact.rs", 1, "PEM armor \"BEGIN\", not SQL"),
    ("semantic.rs", 3, "ensure_schema is read-path setup DDL (prune runs it under store::guarded); embed_pending under store::begin; prune under store::guarded"),
    ("store.rs", 29, "open path (open_once re-checks under the lock); guarded, begin and begin_shared themselves; the rest under store::begin or store::guarded"),
    ("store_atlas.rs", 26, "under store::begin, or a Transaction followed by store::guard"),
    ("tokens.rs", 9, "TOKEN_TABLE is read-path setup DDL; every log row under store::guarded"),
    ("tools_memory.rs", 3, "after_write: BEGIN IMMEDIATE then memory::check_locked before the exports, then COMMIT"),
    ("transcripts.rs", 9, "index: one store::begin_shared transaction holds the tables, a rewrite's deletes and the turns; get, search and path_of TABLES are read-path setup DDL"),
];

/// The non-test part of a source file: up to its first `#[cfg(test)]` module,
/// without comment lines.
fn non_test(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 && lines[i - 1].trim_end() == "#[cfg(test)]" && ["mod ", "pub mod ", "pub(crate) mod "].iter().any(|p| l.starts_with(p)) {
            break;
        }
        if !l.trim_start().starts_with("//") {
            out.push(*l);
        }
    }
    out
}

fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, usize>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, root, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            let n: usize = non_test(&text).iter().map(|l| PATTERNS.iter().map(|pat| l.matches(pat).count()).sum::<usize>()).sum();
            if n > 0 {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.insert(rel, n);
            }
        }
    }
}

#[test]
fn every_raw_write_site_is_listed_with_its_guard() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = BTreeMap::new();
    walk(&src, &src, &mut found);
    let listed: BTreeMap<String, usize> = SITES.iter().map(|(f, n, _)| ((*f).to_string(), *n)).collect();
    let mut wrong = Vec::new();
    for (f, n) in &found {
        match listed.get(f) {
            None => wrong.push(format!("{f}: {n} raw write calls, not listed")),
            Some(m) if m != n => wrong.push(format!("{f}: {n} raw write calls, listed {m}")),
            _ => {}
        }
    }
    for (f, m) in &listed {
        if !found.contains_key(f) {
            wrong.push(format!("{f}: listed {m}, none found"));
        }
    }
    assert!(
        wrong.is_empty(),
        "check that each new write to either store goes through a guard (see the module docs), then update its row in SITES:\n{}",
        wrong.join("\n")
    );
}
