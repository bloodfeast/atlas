//! `atlas ideas`: the captures, read and closed by hand.
//!
//! ```text
//! atlas ideas [list] [--unsure|--all]      the newest captures: id, state, source, score, title
//! atlas ideas show ID                      one capture's exact words
//! atlas ideas file TEXT...                 files TEXT as an idea, redacted as a capture is
//! atlas ideas dismiss ID...                a false capture: closes it, label 0, source dismiss
//! atlas ideas answer ID yes|no             what a question's answer does, by hand, source manual
//! atlas ideas convert ID ruling --by NAME  files a ruling with a ref to it, then closes it
//! atlas ideas convert ID queue [--kind K]  files a queue item naming it, then closes it
//! atlas ideas labels                       labels for a later refit, in eval.rs's shape: id, label,
//!                                          source, then time, asked day, score, weights sha; no text
//! ```

use std::path::Path;
use std::process::ExitCode;

use super::{drain, key_of, set_label, spool_dir, Capture, State, DISMISS_SOURCE, ENTRY, KEY};
use crate::memory::{Head, MemoryStore};
use crate::queue::meta_value;

/// The usage of `atlas ideas`.
pub const USAGE: &str = "usage: atlas ideas [list [--unsure|--all] [--limit N] | show ID | file TEXT... | dismiss ID... | answer ID yes|no | convert ID ruling --by NAME | convert ID queue [--kind K] | labels]";

/// How many rows `list` prints.
const LIST_MAX: usize = 50;
/// The source of a label given at the command line or by a conversion.
const MANUAL_SOURCE: &str = "manual";

type Ruling<'a> = &'a mut dyn FnMut(Vec<String>) -> Result<ExitCode, String>;

fn id_of_key(k: &str) -> &str {
    k.strip_prefix(KEY).unwrap_or(k)
}

/// A capture by its id or key.
fn find(store: &MemoryStore, id: Option<&String>) -> Result<Head, String> {
    let id = id.ok_or(USAGE)?;
    let key = if id.starts_with(KEY) { id.clone() } else { key_of(id) };
    let h = store.head(&key).map_err(|e| e.to_string())?;
    h.filter(|h| h.sub == ENTRY).ok_or_else(|| format!("no idea {id:?}"))
}

/// One capture's key, status, time, title and meta.
type Row = (String, String, String, String, String);

/// The captures, newest first.
fn rows(store: &MemoryStore) -> Result<Vec<Row>, String> {
    let s = |e: rusqlite::Error| e.to_string();
    let mut st = store
        .conn()
        .prepare_cached("SELECT r.key, r.status, v.time, v.title, v.meta FROM records r JOIN versions v ON v.vid = r.head WHERE r.kind = 'journal' AND r.sub = 'idea' ORDER BY r.rid DESC")
        .map_err(s)?;
    let out = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .map_err(s)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(s)?;
    Ok(out)
}

/// Runs `atlas ideas`. `ruling` runs `atlas ruling` with the given arguments.
///
/// # Errors
///
/// A message for a bad command or a store error.
pub fn cli(args: &[String], store: &mut MemoryStore, path: &Path, ruling: Ruling<'_>) -> Result<ExitCode, String> {
    let s = |e: crate::Error| e.to_string();
    // A person waits gladly: this takes the store's full wait, and a capture a
    // hook spooled is in before the list is read.
    drain(store, &spool_dir(path), usize::MAX).map_err(s)?;
    let word = args.first().map_or("list", String::as_str);
    match word {
        "list" => {
            let want_all = args.iter().any(|a| a == "--all");
            let unsure = args.iter().any(|a| a == "--unsure");
            let all = rows(store)?;
            let keep = |status: &str, meta: &str| {
                let st = meta_value(meta, "state").unwrap_or("");
                want_all || (status == "active" && if unsure { st == State::Unsure.name() } else { st == State::Pending.name() })
            };
            let shown: Vec<_> = all.iter().filter(|r| keep(&r.1, &r.4)).collect();
            println!("{} {}", shown.len(), if want_all { "in all" } else if unsure { "unsure" } else { "pending" });
            let limit = match flag(args, "--limit") {
                Some(n) => n.parse().map_err(|_| USAGE.to_string())?,
                None if want_all => usize::MAX,
                None => LIST_MAX,
            };
            for r in shown.iter().take(limit) {
                let m = &r.4;
                println!("{} {} {} {} {} {}", id_of_key(&r.0), &r.2[..r.2.len().min(10)], meta_value(m, "state").unwrap_or("?"), meta_value(m, "source").unwrap_or("?"), meta_value(m, "score").unwrap_or("?"), r.3);
            }
            if shown.len() > limit {
                println!("+{} older; --limit N shows more", shown.len() - limit);
            }
            Ok(ExitCode::SUCCESS)
        }
        "show" => {
            let h = find(store, args.get(1))?;
            println!("{} ({})\n{}\n\n{}", h.key, h.status, h.meta, h.body);
            Ok(ExitCode::SUCCESS)
        }
        "file" => {
            if crate::redact::denylist_unreadable() {
                return Err("ATLAS_LEAK_DENYLIST cannot be read: nothing filed".into());
            }
            let raw = args[1..].join(" ");
            if raw.trim().is_empty() {
                return Err(USAGE.into());
            }
            let judged = |_: &str| Some((0.0, State::Pending, false));
            let c = Capture::new(&raw, "manual", "", judged, crate::redact::denylist()).ok_or(USAGE)?;
            let new = super::file(store, &c).map_err(s)?;
            set_label(store, &c.id, true, MANUAL_SOURCE).map_err(s)?;
            println!("{} {}", if new { "filed" } else { "already filed" }, c.id);
            Ok(ExitCode::SUCCESS)
        }
        "dismiss" => {
            if args.len() < 2 {
                return Err(USAGE.into());
            }
            for a in &args[1..] {
                let h = find(store, Some(a))?;
                let id = id_of_key(&h.key).to_string();
                set_label(store, &id, false, DISMISS_SOURCE).map_err(s)?;
                println!("dismissed {id}");
            }
            Ok(ExitCode::SUCCESS)
        }
        "answer" => {
            let h = find(store, args.get(1))?;
            let yes = match args.get(2).map(String::as_str) {
                Some("yes") => true,
                Some("no") => false,
                _ => return Err(USAGE.into()),
            };
            let id = id_of_key(&h.key).to_string();
            set_label(store, &id, yes, MANUAL_SOURCE).map_err(s)?;
            println!("{id} label {} ({MANUAL_SOURCE})", u8::from(yes));
            Ok(ExitCode::SUCCESS)
        }
        "convert" => convert(args, store, ruling),
        "labels" => {
            print!("{}", labels(store)?);
            Ok(ExitCode::SUCCESS)
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command {other:?}\n{USAGE}")),
    }
}

/// The labels as a file `eval.rs` reads: id, label, source first. An asked row
/// was chosen by band, not at random; its asked day says so. No text.
fn labels(store: &MemoryStore) -> Result<String, String> {
    let mut out = String::new();
    for r in rows(store)?.iter().rev() {
        let m = &r.4;
        if let Some(l) = meta_value(m, "label") {
            let v = |k: &str| meta_value(m, k).unwrap_or("");
            out.push_str(&format!("{}\t{l}\t{}\t{}\t{}\t{}\t{}\n", id_of_key(&r.0), v("label_source"), v("label_time"), v("asked"), v("score"), v("weights")));
        }
    }
    Ok(out)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Files the ruling or queue item first, so a failure leaves the idea pending.
fn convert(args: &[String], store: &mut MemoryStore, ruling: Ruling<'_>) -> Result<ExitCode, String> {
    let s = |e: crate::Error| e.to_string();
    let h = find(store, args.get(1))?;
    if h.status != "active" {
        return Err(format!("{} is {}, not pending", h.key, h.status));
    }
    let id = id_of_key(&h.key).to_string();
    let reason = match args.get(2).map(String::as_str) {
        Some("ruling") => {
            let by = flag(args, "--by").ok_or(USAGE)?;
            // A ruling is one line, and a capture may be several.
            let words = h.body.split_whitespace().collect::<Vec<_>>().join(" ");
            let code = ruling(vec!["--by".into(), by, "--ref".into(), h.key.clone(), "--".into(), words])?;
            if code != ExitCode::SUCCESS {
                return Ok(code);
            }
            "converted to a ruling".to_string()
        }
        Some("queue") => {
            let kind = match flag(args, "--kind") {
                Some(k) => crate::orchestrate::Kind::parse(&k).ok_or_else(|| format!("unknown kind {k:?}"))?,
                None => crate::orchestrate::Kind::parse("builder").ok_or("no builder kind")?,
            };
            let meta = format!("idea: {}", h.key);
            let launch = crate::queue::Launch { kind, lane: None, description: &h.title, subagent_type: "", prompt: &h.body, session: "", class_meta: &meta };
            let (key, _) = crate::queue::add(store, &launch).map_err(s)?;
            println!("queued {key}");
            format!("converted to {key}")
        }
        _ => return Err(USAGE.into()),
    };
    set_label(store, &id, true, MANUAL_SOURCE).map_err(s)?;
    let mut w = store.writer().map_err(s)?;
    w.retire(&h.key, &reason).map_err(s)?;
    w.commit().map_err(s)?;
    println!("{id}: {reason}");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(tag: &str) -> (std::path::PathBuf, MemoryStore) {
        let d = std::env::temp_dir().join(format!("atlas-ideas-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("memory.sqlite");
        let s = MemoryStore::open(&p).unwrap();
        (p, s)
    }

    fn run(args: &[&str], s: &mut MemoryStore, p: &Path, r: Ruling<'_>) -> Result<ExitCode, String> {
        let a: Vec<String> = args.iter().map(ToString::to_string).collect();
        cli(&a, s, p, r)
    }

    fn only_id(s: &MemoryStore) -> String {
        id_of_key(&rows(s).unwrap()[0].0).to_string()
    }

    #[test]
    fn file_dismiss_and_labels_hold_ids_and_labels_only() {
        let (p, mut s) = fresh("dismiss");
        let mut no = |_: Vec<String>| -> Result<ExitCode, String> { panic!("no ruling here") };
        run(&["file", "an", "invented", "plan"], &mut s, &p, &mut no).unwrap();
        let id = only_id(&s);
        run(&["dismiss", &id], &mut s, &p, &mut no).unwrap();
        let h = s.head(&key_of(&id)).unwrap().unwrap();
        assert_eq!((h.status.as_str(), meta_value(&h.meta, "label"), meta_value(&h.meta, "label_source")), ("retired", Some("0"), Some("dismiss")));
        let out = labels(&s).unwrap();
        let f: Vec<&str> = out.trim_end_matches('\n').split('\t').collect();
        assert_eq!((f[0], f[1], f[2], f.len()), (id.as_str(), "0", "dismiss", 7), "{out}");
        assert!(!out.contains("invented"), "{out}");
        assert!(run(&["dismiss", "nope"], &mut s, &p, &mut no).is_err());
        assert!(run(&["bogus"], &mut s, &p, &mut no).is_err());
    }

    #[test]
    fn a_capture_of_several_lines_converts_to_a_one_line_ruling() {
        let (p, mut s) = fresh("convert-lines");
        let mut one_line = |a: Vec<String>| -> Result<ExitCode, String> {
            if a.first().is_some_and(|w| w == "--by") {
                assert_eq!(a.last().map(String::as_str), Some("two lines of words"));
            }
            Ok(ExitCode::SUCCESS)
        };
        run(&["file", "two\nlines\n  of words"], &mut s, &p, &mut one_line).unwrap();
        let id = only_id(&s);
        run(&["convert", &id, "ruling", "--by", "someone"], &mut s, &p, &mut one_line).unwrap();
        assert_eq!(s.head(&key_of(&id)).unwrap().unwrap().status, "retired");
    }

    #[test]
    fn list_all_reaches_the_oldest_and_limit_is_read() {
        let (p, mut s) = fresh("list-all");
        let mut no = |_: Vec<String>| -> Result<ExitCode, String> { panic!("no ruling here") };
        for i in 0..(LIST_MAX + 2) {
            run(&["file", &format!("invented plan number {i}")], &mut s, &p, &mut no).unwrap();
        }
        run(&["list", "--all"], &mut s, &p, &mut no).unwrap();
        run(&["list", "--limit", "2"], &mut s, &p, &mut no).unwrap();
        assert!(run(&["list", "--limit", "many"], &mut s, &p, &mut no).is_err());
    }

    #[test]
    fn convert_files_first_and_closes_after() {
        let (p, mut s) = fresh("convert");
        let mut args_seen = Vec::new();
        {
            let mut ok = |a: Vec<String>| -> Result<ExitCode, String> {
                args_seen = a;
                Ok(ExitCode::SUCCESS)
            };
            run(&["file", "a", "plan", "worth", "a", "ruling"], &mut s, &p, &mut ok).unwrap();
            let id = only_id(&s);
            run(&["convert", &id, "ruling", "--by", "someone"], &mut s, &p, &mut ok).unwrap();
            let h = s.head(&key_of(&id)).unwrap().unwrap();
            assert_eq!((h.status.as_str(), meta_value(&h.meta, "label")), ("retired", Some("1")));
            assert!(run(&["convert", &id, "ruling", "--by", "someone"], &mut s, &p, &mut ok).is_err());
        }
        assert_eq!(args_seen[..4], ["--by", "someone", "--ref", &key_of(&only_id(&s))]);
        // A failed ruling leaves the idea pending.
        let mut bad = |_: Vec<String>| -> Result<ExitCode, String> { Err("refused".into()) };
        run(&["file", "another", "plan", "here"], &mut s, &p, &mut bad).unwrap();
        let id = only_id(&s);
        assert!(run(&["convert", &id, "ruling", "--by", "x"], &mut s, &p, &mut bad).is_err());
        assert_eq!(s.head(&key_of(&id)).unwrap().unwrap().status, "active");
        // To the queue: the item names the idea.
        run(&["convert", &id, "queue"], &mut s, &p, &mut bad).unwrap();
        assert_eq!(s.head(&key_of(&id)).unwrap().unwrap().status, "retired");
        let q = crate::queue::waiting(&s).unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(meta_value(&q[0].meta, "idea"), Some(key_of(&id).as_str()));
    }
}
