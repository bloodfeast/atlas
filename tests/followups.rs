//! Follow-ups from the first live use: link text, short wikilinks, raw group
//! labels, inline counts, preamble edits and line-ending preservation.

mod common;

use std::fs;

use common::*;
use atlas::json;

const P1: &str = "Ideas/PI-001 Cache inside the loader.md";

fn region_body(text: &str, name: &str) -> String {
    let start = text.find(&format!("<!-- vault-mcp:index {name} start")).unwrap();
    let open_end = start + text[start..].find("-->").unwrap() + 3;
    let end = text.find(&format!("<!-- vault-mcp:index {name} end -->")).unwrap();
    text[open_end..end].to_string()
}

#[test]
fn link_text_title_prefers_frontmatter_and_file_forces_the_name() {
    let fx = Fx::new("f-title");
    let mut s = fx.server();
    let spec = |lt: &str| json!({"sources": [{"vault": "perf", "folder": "Ideas"}], "format": "list", "link_text": lt, "sort": "id"});
    call(&mut s, "build_index", json!({"vault": "perf", "path": "T.md", "name": "t", "spec": spec("title")}));
    call(&mut s, "build_index", json!({"vault": "perf", "path": "T.md", "name": "f", "spec": spec("file")}));
    let t = fs::read_to_string(fx.perf().join("T.md")).unwrap();
    assert!(region_body(&t, "t").contains("- [[PI-001 Cache inside the loader|Cache inside the loader]]\n"), "{t}");
    assert!(region_body(&t, "t").contains("- [[PI-002 Throttle|Throttle]]\n"), "{t}");
    assert!(region_body(&t, "f").contains("- [[PI-001 Cache inside the loader]]\n"), "{t}");
    // A note without a frontmatter title falls back to its file name.
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "T.md", "name": "root",
        "spec": {"sources": [{"vault": "perf", "glob": "Index.md"}], "format": "list", "link_text": "title"}}),
    );
    let t = fs::read_to_string(fx.perf().join("T.md")).unwrap();
    assert_eq!(region_body(&t, "root"), "\n- [[Index]]\n");
}

#[test]
fn wikilinks_drop_the_folder_only_when_the_name_is_unique() {
    let fx = Fx::new("f-short");
    write(&fx.perf().join("Areas/Dup.md"), "# a\n");
    write(&fx.perf().join("Ideas/Dup.md"), "# b\n");
    let mut s = fx.server();
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "L.md", "name": "l",
        "spec": {"sources": [{"vault": "perf"}], "format": "list", "sort": "path"}}),
    );
    let t = fs::read_to_string(fx.perf().join("L.md")).unwrap();
    assert!(t.contains("- [[Areas/Dup|Dup]]\n") && t.contains("- [[Ideas/Dup|Dup]]\n"), "{t}");
    assert!(t.contains("- [[GPU passes]]\n") && t.contains("- [[PI-002 Throttle]]\n"), "{t}");
    let r = call(&mut s, "make_link", json!({"from_vault": "perf", "to_vault": "perf", "path": "Ideas/Dup.md"}));
    assert_eq!(r["link"], json!("[[Ideas/Dup]]"));
    let r = call(&mut s, "make_link", json!({"from_vault": "perf", "to_vault": "perf", "path": "Areas/GPU passes.md"}));
    assert_eq!(r["link"], json!("[[GPU passes]]"));
    // Short links still resolve, so nothing reads as broken.
    assert_eq!(call(&mut s, "broken_links", json!({"vault": "perf"}))["total"], json!(1), "only the fixture's [[LOADER-WORK]]");
}

#[test]
fn group_labels_are_raw_unless_title_case_is_asked_for() {
    let fx = Fx::new("f-labels");
    let mut s = fx.server();
    let spec = |label: Option<&str>| {
        let mut g = json!({"key": "status", "order": ["in flight", "parked"]});
        if let Some(l) = label {
            g["label"] = json!(l);
        }
        json!({"sources": [{"vault": "perf", "folder": "Ideas"}], "group_by": g, "format": "counts"})
    };
    call(&mut s, "build_index", json!({"vault": "perf", "path": "G.md", "name": "raw", "spec": spec(None)}));
    call(&mut s, "build_index", json!({"vault": "perf", "path": "G.md", "name": "tc", "spec": spec(Some("title_case"))}));
    let t = fs::read_to_string(fx.perf().join("G.md")).unwrap();
    assert!(region_body(&t, "raw").contains("| in flight | 1 |\n| parked | 1 |"), "{t}");
    assert!(region_body(&t, "tc").contains("| In Flight | 1 |\n| Parked | 1 |"), "{t}");
}

#[test]
fn inline_counts_live_inside_prose() {
    let fx = Fx::new("f-inline");
    let mut s = fx.server();
    let spec = json!({"sources": [{"vault": "perf", "folder": "Ideas", "glob": "PI-*.md"}], "format": "count_inline"});
    call(&mut s, "create_note", json!({"vault": "perf", "path": "Summary.md", "content": "# Summary\n"}));
    call(&mut s, "build_index", json!({"vault": "perf", "path": "Summary.md", "name": "ideas", "spec": spec}));
    let t = fs::read_to_string(fx.perf().join("Summary.md")).unwrap();
    let marker = t.lines().last().unwrap().to_string();
    assert!(marker.starts_with("<!-- vault-mcp:index ideas start {") && marker.ends_with(" -->2<!-- vault-mcp:index ideas end -->"), "{t}");
    // Move it into a sentence by hand, next to a block index on the same note.
    let prose = format!("# Summary\n\nWe track {marker} ideas, all listed below.\n\n");
    fs::write(fx.perf().join("Summary.md"), &prose).unwrap();
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "Summary.md", "name": "list",
        "spec": {"sources": [{"vault": "perf", "folder": "Ideas"}], "format": "list"}}),
    );
    call(&mut s, "create_note", json!({"vault": "perf", "path": "Ideas/PI-003 New.md", "content": "# PI-003\n"}));
    let r = call(&mut s, "refresh_index", json!({"vault": "perf", "path": "Summary.md"}));
    assert_eq!(r["changed"], json!(true));
    let t = fs::read_to_string(fx.perf().join("Summary.md")).unwrap();
    let line = t.lines().find(|l| l.starts_with("We track ")).unwrap();
    assert!(line.ends_with(" -->3<!-- vault-mcp:index ideas end --> ideas, all listed below."), "{line}");
    assert!(region_body(&t, "list").contains("- [[PI-003 New]]"));
    let r = call(&mut s, "refresh_index", json!({"vault": "perf", "path": "Summary.md"}));
    assert_eq!(r["changed"], json!(false));
}

#[test]
fn replace_section_addresses_the_preamble() {
    let fx = Fx::new("f-preamble");
    let mut s = fx.server();
    let p = fx.perf().join(P1);
    let before = fs::read_to_string(&p).unwrap();
    let sha = read(&mut s, "perf", P1)["sha256"].as_str().unwrap().to_string();
    call(
        &mut s,
        "replace_section",
        json!({"vault": "perf", "path": P1, "preamble": true, "content": "Summary: a cache fix.", "expected_sha256": sha}),
    );
    let after = fs::read_to_string(&p).unwrap();
    let fm_end = before.find("---\n\n# PI-001").unwrap() + 4;
    assert_eq!(after, format!("{}\nSummary: a cache fix.\n\n{}", &before[..fm_end], &before[fm_end + 1..]));
    // heading "" is the same thing; the sha is still required and checked.
    let e =
        call_err(&mut s, "replace_section", json!({"vault": "perf", "path": P1, "heading": "", "content": "x", "expected_sha256": sha}));
    assert!(e.contains("stale edit"), "{e}");
    let sha = read(&mut s, "perf", P1)["sha256"].as_str().unwrap().to_string();
    call(&mut s, "replace_section", json!({"vault": "perf", "path": P1, "heading": "", "content": "", "expected_sha256": sha}));
    assert_eq!(fs::read_to_string(&p).unwrap(), before, "an empty preamble restores the original layout");
    let e = call_err(&mut s, "replace_section", json!({"vault": "perf", "path": P1, "content": "x", "expected_sha256": sha}));
    assert!(e.contains("preamble"), "{e}");
    // A note with no frontmatter and no heading is all preamble.
    call(&mut s, "create_note", json!({"vault": "perf", "path": "Plain.md", "content": "old text\n"}));
    let sha = read(&mut s, "perf", "Plain.md")["sha256"].as_str().unwrap().to_string();
    call(
        &mut s,
        "replace_section",
        json!({"vault": "perf", "path": "Plain.md", "preamble": true, "content": "new text", "expected_sha256": sha}),
    );
    assert_eq!(fs::read_to_string(fx.perf().join("Plain.md")).unwrap(), "new text\n");
}

#[test]
fn writes_keep_crlf_files_crlf() {
    let fx = Fx::new("f-crlf");
    let crlf = IDEA_1.replace('\n', "\r\n");
    write(&fx.perf().join("Ideas/PI-009 Crlf.md"), &crlf);
    let mut s = fx.server();
    let p = "Ideas/PI-009 Crlf.md";
    call(&mut s, "append_history", json!({"vault": "perf", "path": p, "text": "a history line", "date": "2026-10-09"}));
    call(&mut s, "append_to_note", json!({"vault": "perf", "path": p, "text": "two\nlines"}));
    call(&mut s, "set_frontmatter", json!({"vault": "perf", "path": p, "values": {"status": "parked"}}));
    call(&mut s, "link_notes", json!({"from_vault": "perf", "from_path": p, "to_vault": "campaign", "to_path": "Missions/M1 Hold.md"}));
    let sha = read(&mut s, "perf", p)["sha256"].as_str().unwrap().to_string();
    call(
        &mut s,
        "replace_section",
        json!({"vault": "perf", "path": p, "heading": "Entry", "content": "- new\n- entry", "expected_sha256": sha}),
    );
    let sha = read(&mut s, "perf", p)["sha256"].as_str().unwrap().to_string();
    call(&mut s, "replace_section", json!({"vault": "perf", "path": p, "preamble": true, "content": "pre\namble", "expected_sha256": sha}));
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": p, "name": "x", "spec": {"sources": [{"vault": "perf", "folder": "Areas"}], "format": "list"}}),
    );
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": p, "name": "n", "spec": {"sources": [{"vault": "perf"}], "format": "count_inline"}}),
    );
    let t = fs::read_to_string(fx.perf().join(p)).unwrap();
    assert!(!t.replace("\r\n", "").contains('\n'), "a bare LF was written into a CRLF note:\n{t:?}");
    assert!(t.contains("- 2026-10-09: a history line\r\n") && t.contains("pre\r\namble\r\n") && t.contains("- new\r\n- entry\r\n"));
}
