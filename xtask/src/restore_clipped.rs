//! `cargo xtask restore-clipped --store FILE --from JSON`: a one-shot
//! repair of the journal entries the title clip cut (a first line over
//! [`CLIP_CHARS`] characters lost everything past it). The JSON maps each key
//! to its `lane`, `entry` and full `text`, recovered from the transcripts
//! (other fields are ignored). Each entry that is still clipped (a
//! [`CLIP_CHARS`]-character title, an empty body) is amended in place
//! (`Writer::amend_journal`): the title by the store's rule
//! (`atlas::memory::short_title`), the full text as its body, and its meta
//! and refs as they were.
//!
//! An entry is skipped when it is no journal entry (a handoff cannot be
//! amended), when it is no longer clipped, when its text is no longer than
//! its title (nothing was cut), when the text does not start with the stored
//! title, or when its lane or entry differ. All of it is one transaction.
//! There is no dry-run flag: run it on a copy of the store first.

use std::path::Path;

use atlas::json::{self, Value};
use atlas::memory::{short_title, Draft, MemoryStore};

/// The length the title clip cut first lines to (journals from b5304ff,
/// handoffs from 39db6c9, until #103 kept the long line in the body).
pub const CLIP_CHARS: usize = 200;

/// What one run did.
#[derive(Debug, Default)]
pub struct Tally {
    /// Amended in place.
    pub restored: Vec<String>,
    /// Text no longer than the title: nothing was cut.
    pub intact: Vec<String>,
    /// No longer a 200-character title with an empty body.
    pub not_clipped: Vec<String>,
    /// Lane, entry or title differ from the store's.
    pub mismatch: Vec<String>,
    /// Not in the store.
    pub missing: Vec<String>,
    /// Not a journal entry (a handoff): amend cannot rewrite it.
    pub not_journal: Vec<String>,
}

/// Applies the JSON to the store.
///
/// # Errors
///
/// A message when the JSON or the store cannot be read, or a write fails.
pub fn run(store: &Path, from: &Path) -> Result<Tally, String> {
    let text = std::fs::read_to_string(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let v = json::parse(&text).map_err(|e| format!("{}: {e}", from.display()))?;
    let Value::Object(map) = v else { return Err(format!("{}: not a JSON object", from.display())) };
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    let mut m = MemoryStore::open(store).map_err(|e| e.to_string())?;
    let mut t = Tally::default();
    let mut w = m.writer().map_err(|e| e.to_string())?;
    for key in keys {
        let Some(e) = map.get(&key) else { continue };
        let field = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let full = field("text");
        let full = full.trim();
        let Some(h) = rusqlite::OptionalExtension::optional(w.head(&key)).map_err(|e| format!("{key}: {e}"))? else {
            t.missing.push(key);
            continue;
        };
        if h.kind != "journal" {
            t.not_journal.push(key);
            continue;
        }
        if h.title.chars().count() != CLIP_CHARS || !h.body.trim().is_empty() {
            t.not_clipped.push(key);
            continue;
        }
        if full.chars().count() <= CLIP_CHARS {
            t.intact.push(key);
            continue;
        }
        if !full.starts_with(h.title.as_str()) || h.scope != field("lane") || h.sub != field("entry") {
            t.mismatch.push(key);
            continue;
        }
        let d = Draft {
            title: short_title(full),
            body: format!("{full}\n"),
            reason: "restore of an entry the 200-character title clip cut (TITLE-CLIP)".into(),
            author: "atlas-restore".into(),
            client: "xtask restore-clipped".into(),
            ..Draft::default()
        };
        w.amend_journal(&key, &h.sha, &h.scope, &h.sub, &d).map_err(|e| format!("{key}: {e}"))?;
        t.restored.push(key);
    }
    w.commit().map_err(|e| e.to_string())?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(store: &mut MemoryStore, key: &str, kind: &str, sub: &str, lane: &str, title: &str, body: &str) {
        let d = Draft { title: title.into(), body: body.into(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Draft::default() };
        let mut w = store.writer().unwrap();
        w.create(key, kind, sub, lane, &d, None).unwrap();
        w.commit().unwrap();
    }

    #[test]
    fn run_restores_each_clipped_entry_once_and_skips_the_rest() {
        let d = std::env::temp_dir().join(format!("atlas-restore-clipped-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let db = d.join("memory.sqlite");
        let full = format!("{}. And the tail the clip lost.", "word ".repeat(50).trim());
        let clipped: String = full.chars().take(CLIP_CHARS).collect();
        let mut m = MemoryStore::open(&db).unwrap();
        seed(&mut m, "journal:L:0001", "journal", "note", "L", &clipped, "");
        seed(&mut m, "journal:L:0002", "journal", "note", "L", &clipped, "");
        seed(&mut m, "journal:L:0003", "journal", "note", "L", "short", "");
        seed(&mut m, "handoff:L:001", "handoff", "", "L", &clipped, "");
        drop(m);
        let entry = |lane: &str, entry: &str| format!("{{\"lane\": \"{lane}\", \"entry\": \"{entry}\", \"text\": {}}}", json::to_string(&Value::String(full.clone())));
        let from = d.join("restore.json");
        let text = format!(
            "{{\"journal:L:0001\": {}, \"journal:L:0002\": {}, \"journal:L:0003\": {}, \"handoff:L:001\": {}, \"journal:L:0009\": {}}}",
            entry("L", "note"),
            entry("L", "decision"),
            entry("L", "note"),
            entry("L", ""),
            entry("L", "note")
        );
        std::fs::write(&from, text).unwrap();
        let t = run(&db, &from).unwrap();
        assert_eq!(t.restored, ["journal:L:0001"]);
        assert_eq!(t.mismatch, ["journal:L:0002"]);
        assert_eq!(t.not_clipped, ["journal:L:0003"]);
        assert_eq!(t.not_journal, ["handoff:L:001"]);
        assert_eq!(t.missing, ["journal:L:0009"]);
        let m = MemoryStore::open(&db).unwrap();
        let h = m.head("journal:L:0001").unwrap().unwrap();
        assert_eq!(h.body.trim(), full);
        assert!(h.title.chars().count() <= atlas::memory::TITLE_CHARS, "{}", h.title);
        drop(m);
        // A second run finds nothing left to restore.
        let again = run(&db, &from).unwrap();
        assert!(again.restored.is_empty() && again.not_clipped.contains(&"journal:L:0001".to_string()), "{again:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
