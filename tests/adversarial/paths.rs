//! Confinement beyond tests/confinement.rs: verbatim and device prefixes,
//! mixed separators, every Windows device name, alternate data streams,
//! length limits, Unicode normalisation and case collisions, hard links,
//! and every write tool aimed out of its root. Also the capped reads and
//! atomic writes of `fsio`.

use std::fs;
use std::path::Path;

use crate::common::*;
use atlas::fsio;
use atlas::json;
use atlas::paths::{split_rel, Kind};

const ESCAPES: &[&str] = &[
    "\\\\?\\C:\\x.md",
    "\\\\?\\UNC\\server\\share\\x.md",
    "\\\\.\\PhysicalDrive0",
    "\\\\.\\pipe\\x.md",
    "//?/C:/x.md",
    "//server/share/x.md",
    "Ideas\\..\\..\\x.md",
    "Ideas/..\\../x.md",
    "Ideas\\../Index.md",
    "a/./b.md",
    "C:/x.md",
    "c:x.md",
    "x.md:Zone.Identifier",
    "x.md:$DATA",
    "Ideas:x/y.md",
    "x.md\u{0}.md",
    "x\u{1}.md",
    "x\u{7f}.md",
    "x\u{85}.md",
    "x.md\t",
    "x.md\n",
    " /x.md",
    "x .md/",
    "Ideas./x.md",
    "Ideas /x.md",
    "x.md...",
    "x.md. . .",
];

#[test]
fn lexical_escapes_are_refused() {
    for p in ESCAPES {
        assert!(split_rel(p, Kind::Note).is_err(), "{p:?} accepted as a note path");
        assert!(split_rel(p, Kind::Folder).is_err() || !p.contains(".."), "{p:?} accepted as a folder");
    }
    assert!(split_rel("a/b/", Kind::Folder).is_ok());
    assert!(split_rel("a//", Kind::Folder).is_err());
    assert!(split_rel("a/b/", Kind::Note).is_err());
}

/// Every name Windows maps to a device, with and without an extension,
/// in any case, with trailing spaces before the extension.
#[test]
fn every_device_name_is_refused() {
    let mut names: Vec<String> = ["CON", "PRN", "AUX", "NUL"].iter().map(|s| s.to_string()).collect();
    for n in 1..=9 {
        names.push(format!("COM{n}"));
        names.push(format!("LPT{n}"));
    }
    for name in &names {
        for form in [format!("{name}.md"), format!("{}.md", name.to_lowercase()), format!("{name} .md"), format!("{name}.x.md"), format!("{name}/a.md"), format!("a/{name}.md")] {
            assert!(split_rel(&form, Kind::Note).is_err(), "{form:?} accepted");
        }
    }
    // Ordinary names that start like a device are served.
    for ok in ["CONTRACT.md", "console.md", "COM10.md", "LPT.md", "nullable.md", "AUXILIARY/a.md"] {
        assert!(split_rel(ok, Kind::Note).is_ok(), "{ok:?} refused");
    }
}

/// Windows also maps the superscript-digit ports and the console buffers
/// to devices.
#[test]
fn superscript_ports_and_console_buffers_are_refused() {
    for form in ["COM\u{b9}.md", "COM\u{b2}.md", "COM\u{b3}.md", "LPT\u{b9}.md", "lpt\u{b2}.md", "CONIN$.md", "CONOUT$.md", "conin$/a.md"] {
        assert!(split_rel(form, Kind::Note).is_err(), "{form:?} accepted");
    }
}

#[test]
fn path_length_limits() {
    let long = "a".repeat(atlas::paths::MAX_REL_PATH - 3) + ".md";
    assert_eq!(long.len(), atlas::paths::MAX_REL_PATH);
    assert!(split_rel(&long, Kind::Note).is_ok());
    assert!(split_rel(&format!("a{long}"), Kind::Note).is_err());
    // Multi-byte: the cap is bytes, not characters.
    let wide = "\u{1f980}".repeat(130) + ".md";
    assert!(split_rel(&wide, Kind::Note).is_err());
    // A legal relative path that is too long for the file system is an
    // error, not a panic, for reads and writes.
    let fx = Fx::new("adv-long");
    let mut s = fx.server();
    let deep = (0..40).map(|i| format!("folder{i:03}")).collect::<Vec<_>>().join("/") + "/n.md";
    assert!(deep.len() <= atlas::paths::MAX_REL_PATH);
    let r = s.call("create_note", &json!({"vault": "perf", "path": deep, "content": "x", "create_folders": true, "reason": TEST_REASON}));
    if r.is_ok() {
        assert_eq!(read(&mut s, "perf", &deep)["content"], "x");
    }
    let name = "n".repeat(300) + ".md";
    let e = s.call("create_note", &json!({"vault": "perf", "path": name, "content": "x", "reason": TEST_REASON}));
    assert!(e.is_err(), "a 303-byte file name was created");
}

/// Case and Unicode normalisation collisions never overwrite: create is
/// exclusive on every file system, and a differently cased read finds the
/// stored name.
#[test]
fn case_and_normalisation_collisions_do_not_overwrite() {
    let fx = Fx::new("adv-case");
    let mut s = fx.server();
    call(&mut s, "create_note", json!({"vault": "perf", "path": "Caf\u{e9}.md", "content": "nfc"}));
    // NFD spelling: e + combining acute.
    let r = s.call("create_note", &json!({"vault": "perf", "path": "Cafe\u{301}.md", "content": "nfd", "reason": TEST_REASON}));
    let nfc = fs::read_to_string(fx.perf().join("Caf\u{e9}.md")).unwrap();
    assert_eq!(nfc, "nfc", "the NFC note was overwritten ({r:?})");
    let r = s.call("create_note", &json!({"vault": "perf", "path": "INDEX.md", "content": "upper", "reason": TEST_REASON}));
    if cfg!(any(windows, target_os = "macos")) {
        assert!(r.is_err(), "a case collision created a second note on a case-insensitive file system");
    }
    let idx = fs::read_to_string(fx.perf().join("Index.md")).unwrap();
    assert!(idx.contains("# Index"), "Index.md overwritten: {idx}");
    // The Kelvin sign and the Turkish dotted I fold to ASCII letters in
    // some case maps; they are never confused with an existing note.
    for p in ["\u{212a}.md", "\u{130}ndex.md"] {
        let _ = s.call("create_note", &json!({"vault": "perf", "path": p, "content": "fold", "reason": TEST_REASON}));
    }
    assert!(fs::read_to_string(fx.perf().join("Index.md")).unwrap().contains("# Index"));
}

/// A hard link inside a vault to a file outside it: a write replaces the
/// link (temp file and rename), so the outside file is never modified.
#[test]
fn a_hard_link_to_an_outside_file_is_not_written_through() {
    let fx = Fx::new("adv-hardlink");
    let outside = fx.tmp.path().join("outside.md");
    write(&outside, "# Outside\n");
    let link = fx.perf().join("linked.md");
    if fs::hard_link(&outside, &link).is_err() {
        eprintln!("hard links unsupported here; skipped");
        return;
    }
    let mut s = fx.server();
    let _ = s.call("append_to_note", &json!({"vault": "perf", "path": "linked.md", "text": "appended", "reason": TEST_REASON}));
    let _ = s.call("append_history", &json!({"vault": "perf", "path": "linked.md", "text": "h", "date": "2026-01-01", "reason": TEST_REASON}));
    assert_eq!(fs::read_to_string(&outside).unwrap(), "# Outside\n", "the outside file was written through a hard link");
}

/// A symlinked folder that points back up (a loop) and one that points at
/// the vault root do not hang the walker or escape.
#[cfg(unix)]
#[test]
fn link_loops_do_not_hang_the_walker() {
    let fx = Fx::new("adv-loop");
    std::os::unix::fs::symlink(fx.perf(), fx.perf().join("Ideas").join("loop")).unwrap();
    std::os::unix::fs::symlink(fx.tmp.path(), fx.perf().join("up")).unwrap();
    let mut s = fx.server();
    let all = call(&mut s, "list_notes", json!({"vault": "perf"}));
    let n = all["notes"].as_array().unwrap().len();
    assert!(n < 20, "{n} notes: the walker followed a loop");
    let e = s.call("read_note", &json!({"vault": "perf", "path": "up/Field Notes/Missions/M1 Hold.md"}));
    assert!(e.is_err());
}

/// Every write tool aimed at a path outside its vault, or into a dot
/// folder, is refused and writes nothing anywhere.
#[test]
fn every_write_tool_stays_inside_its_root() {
    let fx = Fx::new("adv-writes");
    let mut s = fx.server();
    let bad = ["../canary.md", "..\\canary.md", "Ideas/../../canary.md", ".obsidian/canary.md", "C:/canary.md", "\\\\?\\C:\\canary.md", "canary.md:ads", "/canary.md"];
    let before = snapshot(fx.tmp.path());
    for p in bad {
        let cases = vec![
            ("create_note", json!({"vault": "perf", "path": p, "content": "x", "create_folders": true})),
            ("append_to_note", json!({"vault": "perf", "path": p, "text": "x"})),
            ("append_history", json!({"vault": "perf", "path": p, "text": "x"})),
            ("set_frontmatter", json!({"vault": "perf", "path": p, "values": {"a": "b"}})),
            ("replace_section", json!({"vault": "perf", "path": p, "heading": "H", "content": "x", "expected_sha256": "0".repeat(64)})),
            ("link_notes", json!({"from_vault": "perf", "from_path": p, "to_vault": "perf", "to_path": "Index.md"})),
            ("link_notes", json!({"from_vault": "perf", "from_path": "Index.md", "to_vault": "perf", "to_path": p, "reverse": true})),
            ("build_index", json!({"vault": "perf", "path": p, "name": "x", "spec": {}})),
            ("refresh_index", json!({"vault": "perf", "path": p})),
            ("next_id", json!({"vault": "perf", "folder": p.trim_end_matches(".md"), "prefix": "C-", "content": "x"})),
            ("next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "C-", "name": p, "content": "x"})),
            ("next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": p, "content": "x"})),
            ("open_discussion", json!({"vault": "perf", "topic": p, "subjects": [p], "participants": ["a", "b"], "opening_post": {"author": "a", "kind": "claim", "body": "b"}})),
            ("post", json!({"thread": format!("perf:{p}"), "author": "a", "kind": "claim", "body": "x"})),
            ("conclude_discussion", json!({"thread": format!("perf:{p}"), "verdict_body": "x"})),
            ("mark_unresolved", json!({"thread": format!("perf:{p}"), "summary": "x", "open_questions": ["q"], "positions": ["p"], "settle_by": "2030-01-01"})),
            ("record_decision", json!({"thread": format!("perf:{p}"), "decided_by": "x", "decision": "y"})),
        ];
        for (tool, args) in cases {
            let mut a = args;
            a.as_object_mut().unwrap().insert("reason".into(), json!(TEST_REASON));
            // A tool may accept a hostile string as a title or a name; what
            // it wrote is checked below.
            let _ = s.call(tool, &a);
        }
    }
    let after = snapshot(fx.tmp.path());
    let new: Vec<&String> = after.iter().filter(|f| !before.contains(f)).collect();
    for f in &new {
        let inside = fx.specs.iter().any(|(_, v)| Path::new(f.as_str()).starts_with(v)) || f.contains("state");
        assert!(inside, "a write landed outside the vaults: {f}");
        assert!(!f.contains(".obsidian"), "a write landed in a dot folder: {f}");
    }
    assert!(!fx.tmp.path().join("canary.md").exists());
    assert!(!fx.tmp.path().parent().unwrap().join("canary.md").exists());
}

fn snapshot(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn rec(d: &Path, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                rec(&p, out);
            } else {
                out.push(p.display().to_string());
            }
        }
    }
    rec(root, &mut out);
    out
}

// ------------------------------------------------------------------ fsio

#[test]
fn capped_reads_refuse_large_and_non_utf8_files() {
    let t = Temp::new("adv-fsio");
    let big = t.path().join("big.md");
    fs::write(&big, vec![b'a'; fsio::MAX_READ as usize + 1]).unwrap();
    assert!(matches!(fsio::read_capped(&big), Err(atlas::Error::TooLarge(_))));
    let exact = t.path().join("exact.md");
    fs::write(&exact, vec![b'a'; fsio::MAX_READ as usize]).unwrap();
    assert_eq!(fsio::read_capped(&exact).unwrap().len(), fsio::MAX_READ as usize);
    let bad = t.path().join("bad.md");
    fs::write(&bad, b"ok \xff\xfe not utf-8").unwrap();
    assert!(matches!(fsio::read_text(&bad), Err(atlas::Error::InvalidArgument(_))));
    let empty = t.path().join("empty.md");
    fs::write(&empty, b"").unwrap();
    assert_eq!(fsio::read_text(&empty).unwrap(), "");
    assert!(fsio::read_capped(&t.path().join("missing.md")).is_err());
    assert!(fsio::read_capped(t.path()).is_err(), "a folder is not a note");
}

#[test]
fn atomic_writes_are_capped_exclusive_and_leave_no_temp_files() {
    let t = Temp::new("adv-fsio-w");
    let target = t.path().join("n.md");
    fsio::create_exclusive(&target, b"one").unwrap();
    assert!(matches!(fsio::create_exclusive(&target, b"two"), Err(atlas::Error::AlreadyExists(_))));
    assert_eq!(fs::read(&target).unwrap(), b"one");
    fsio::replace_atomic(&target, b"three").unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"three");
    let huge = vec![b'x'; fsio::MAX_READ as usize + 1];
    assert!(matches!(fsio::replace_atomic(&target, &huge), Err(atlas::Error::TooLarge(_))));
    assert!(matches!(fsio::create_exclusive(&t.path().join("h.md"), &huge), Err(atlas::Error::TooLarge(_))));
    assert_eq!(fs::read(&target).unwrap(), b"three");
    // A missing folder is an error, and no temp file is left anywhere.
    assert!(fsio::replace_atomic(&t.path().join("no").join("n.md"), b"x").is_err());
    assert!(fsio::create_exclusive(&t.path().join("no").join("n.md"), b"x").is_err());
    let left: Vec<_> = fs::read_dir(t.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".tmp")).collect();
    assert!(left.is_empty(), "{left:?}");
}

/// Many threads creating the same note: exactly one wins.
#[test]
fn concurrent_exclusive_creates_have_one_winner() {
    let t = Temp::new("adv-fsio-race");
    let target = t.path().join("race.md");
    let wins: usize = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..16).map(|i| {
            let target = target.clone();
            sc.spawn(move || fsio::create_exclusive(&target, format!("writer {i}").as_bytes()).is_ok() as usize)
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    assert_eq!(wins, 1);
    assert!(fs::read_to_string(&target).unwrap().starts_with("writer "));
}
