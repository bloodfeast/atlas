//! R7: `atlas ruling` and `atlas entry` on an empty project. The record is
//! written, its exact line lands under a heading atlas owns (never a
//! foreign one), and lookup returns the definition at once, from the
//! explicit field rather than the prose.

mod common;

use std::path::Path;
use std::process::Command;

use atlas::memory::MemoryStore;
use atlas::paths::Vaults;
use atlas::store::{DefText, Store};
use common::*;

fn atlas(home: &Path, args: &[&str]) -> (bool, String) {
    let o = Command::new(bin())
        .args(args)
        .env("ATLAS_HOME", home)
        .env_remove("ATLAS_CONFIG")
        .env("CLAUDE_CONFIG_DIR", home.join("claude"))
        .output()
        .unwrap();
    (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn store(home: &Path) -> Store {
    let v = Vaults::new(&[("notes".to_string(), home.join("notes"))]).unwrap();
    Store::open(&home.join("state").join("atlas.sqlite"), v).unwrap()
}

#[test]
fn a_ruling_defines_its_ids_under_its_own_heading() {
    let t = Temp::new("records");
    let home = t.path().join("home");
    // The first run writes the template (an empty `notes` vault, no id patterns).
    let (ok, out) = atlas(&home, &["settings", "list"]);
    assert!(ok, "{out}");
    // A file whose last heading is someone else's.
    let intent = home.join("notes").join("INTENT.md");
    write(&intent, "# Intent\n\n## 2026-10-23 OTHER-THING\n\n- 2026-10-23 Someone: an older ruling.\n");
    let (ok, out) = atlas(&home, &["ruling", "--by", "Owner", "--defines", "ALPHA-LEAN", "Q7", "--date", "2026-10-31", "--", "the", "alpha", "plan", "is", "adopted."]);
    assert!(ok, "{out}");
    assert!(out.contains("indexed: ALPHA-LEAN defined at notes/INTENT.md:9 (Intent > 2026-10-31 rulings (atlas))"), "{out}");
    assert!(out.contains("indexed: Q7 defined"), "an id of no configured shape is still defined: {out}");
    let text = std::fs::read_to_string(&intent).unwrap();
    assert_eq!(
        text,
        "# Intent\n\n## 2026-10-23 OTHER-THING\n\n- 2026-10-23 Someone: an older ruling.\n\n## 2026-10-31 rulings (atlas)\n\n- 2026-10-31 Owner: the alpha plan is adopted. <!-- atlas defines: ALPHA-LEAN Q7 -->\n"
    );
    // A second ruling the same day goes under the same heading.
    let (ok, out) = atlas(&home, &["ruling", "--by", "Owner", "--date", "2026-10-31", "--", "a second ruling."]);
    assert!(ok, "{out}");
    let text = std::fs::read_to_string(&intent).unwrap();
    assert_eq!(text.matches("## 2026-10-31 rulings (atlas)").count(), 1);
    assert!(text.ends_with("-->\n- 2026-10-31 Owner: a second ruling.\n"), "{text}");
    // lookup: the field definition, under the new heading, ranked first.
    let s = store(&home);
    let l = s.lookup("ALPHA-LEAN", 10, DefText::None).unwrap();
    let d = &l.definitions[0];
    assert_eq!((d.path.as_str(), d.line, d.def.as_deref(), d.kind.as_str()), ("INTENT.md", 9, Some("field"), "field"));
    assert_eq!(d.heading_path, "Intent > 2026-10-31 rulings (atlas)");
    assert!(!d.heading_path.contains("OTHER-THING"));
    // The record carries the fields.
    let m = MemoryStore::open(&home.join("state").join("memory.sqlite")).unwrap();
    let h = m.head("ruling:r00001").unwrap().unwrap();
    assert_eq!((h.kind.as_str(), h.body.as_str()), ("rule", "- 2026-10-31 Owner: the alpha plan is adopted. <!-- atlas defines: ALPHA-LEAN Q7 -->\n"));
    let refs: Vec<(String, String)> = m
        .conn()
        .prepare("SELECT f.kind, f.target FROM refs f JOIN records r ON r.rid = f.rid WHERE r.key = 'ruling:r00001' ORDER BY f.target")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(refs, [("defines".to_string(), "ALPHA-LEAN".to_string()), ("defines".to_string(), "Q7".to_string())]);
    // Bad fields write nothing.
    let before = std::fs::read_to_string(&intent).unwrap();
    let (ok, out) = atlas(&home, &["ruling", "--by", "Owner", "--", "hides <!-- a comment -->"]);
    assert!(!ok && out.contains("HTML comment"), "{out}");
    let (ok, out) = atlas(&home, &["ruling", "--", "no author"]);
    assert!(!ok && out.contains("--by is required"), "{out}");
    assert_eq!(std::fs::read_to_string(&intent).unwrap(), before);
}

#[test]
fn an_entry_lands_in_its_own_file_under_its_own_heading() {
    let t = Temp::new("records-entry");
    let home = t.path().join("home");
    let (ok, out) = atlas(&home, &["entry", "--title", "Checkpoint", "--date", "2026-10-31", "--time", "18:05", "--", "two lanes running."]);
    assert!(ok, "{out}");
    let text = std::fs::read_to_string(home.join("notes").join("LOG.md")).unwrap();
    assert_eq!(text, "## 2026-10-31 entries (atlas)\n\n- 2026-10-31 18:05 Checkpoint: two lanes running.\n");
    assert!(out.starts_with("entry:s00001 -> "), "{out}");
    let (ok, out) = atlas(&home, &["entry", "--title", "x", "--time", "6pm", "--", "t"]);
    assert!(!ok && out.contains("HH:MM"), "{out}");
}

/// The documented form, `atlas ruling --by NAME --defines ID ... TEXT`,
/// without `--`: the list flag stops at the quoted text, and when the text
/// is one word the list's last word is the text.
#[test]
fn a_ruling_takes_its_text_after_the_defines_list_without_a_separator() {
    let t = Temp::new("records-nosep");
    let home = t.path().join("home");
    let (ok, out) = atlas(&home, &["settings", "list"]);
    assert!(ok, "{out}");
    let intent = home.join("notes").join("INTENT.md");
    write(&intent, "# Intent\n");
    let (ok, out) = atlas(&home, &["ruling", "--by", "Owner", "--defines", "BETA-ONE", "BETA-TWO", "--date", "2026-11-01", "the beta plan is adopted"]);
    assert!(ok, "{out}");
    let (ok, out2) = atlas(&home, &["ruling", "--by", "Owner", "--defines", "GAMMA-ONE", "--date", "2026-11-01", "Adopted."]);
    assert!(ok, "{out2}");
    let (ok, out3) = atlas(&home, &["ruling", "--by", "Owner", "--date", "2026-11-01", "--defines", "DELTA-ONE", "Adopted"]);
    assert!(ok, "{out3}");
    let text = std::fs::read_to_string(&intent).unwrap();
    assert!(text.contains("- 2026-11-01 Owner: the beta plan is adopted <!-- atlas defines: BETA-ONE BETA-TWO -->\n"), "{text}");
    assert!(text.contains("- 2026-11-01 Owner: Adopted. <!-- atlas defines: GAMMA-ONE -->\n"), "{text}");
    assert!(text.contains("- 2026-11-01 Owner: Adopted <!-- atlas defines: DELTA-ONE -->\n"), "{text}");
}
