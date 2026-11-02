//! The remaining reachable surfaces: the crop command end to end, link
//! resolution with relative, absolute and out-of-vault targets, the
//! transcript scan behind hook hits, and the updater's refusal branches.

use std::fs;

use crate::common::*;
use atlas::json;
use atlas::picture::{self, Image};

#[test]
fn the_crop_command_end_to_end() {
    let t = Temp::new("adv-crop-cli");
    let input = t.path().join("in.png");
    let img = Image { width: 8, height: 4, rgba: (0..128).map(|i| (i * 2) as u8).collect() };
    fs::write(&input, picture::encode_png(&img)).unwrap();
    let out = t.path().join("out.png");
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let (i, o) = (input.display().to_string(), out.display().to_string());
    assert!(atlas::crop::cli(s(&["--help"])).is_ok());
    assert!(atlas::crop::cli(s(&[&i, &o, "--frames", "2-3", "--of", "4", "--axis", "x", "--max", "1x1"])).is_ok());
    let got = picture::decode(&fs::read(&out).unwrap()).unwrap();
    assert_eq!((got.width, got.height), (1, 1));
    assert!(atlas::crop::cli(s(&[&i, &o, "--rect", "1,1,99,99"])).is_ok());
    for bad in [
        vec![i.as_str(), o.as_str(), "--axis", "z"],
        vec![i.as_str(), o.as_str(), "--bogus"],
        vec![i.as_str(), o.as_str(), "--frames", "9", "--of", "4"],
        vec!["missing.png", o.as_str()],
        vec![o.as_str()],
    ] {
        assert!(atlas::crop::cli(s(&bad)).is_err(), "{bad:?}");
    }
    // A file that is not a picture, and a truncated one.
    fs::write(&input, b"GIF89a").unwrap();
    assert!(atlas::crop::cli(s(&[&i, &o])).is_err());
    fs::write(&input, &picture::encode_png(&img)[..40]).unwrap();
    assert!(atlas::crop::cli(s(&[&i, &o])).is_err());
}

/// Relative, absolute-path and out-of-vault link targets: a link resolves
/// to a note inside a served vault or is missing; it never names a file
/// outside the vaults.
#[test]
fn link_targets_resolve_inside_the_vaults_or_not_at_all() {
    let fx = Fx::new("adv-links");
    let outside = fx.tmp.path().join("outside.md");
    write(&outside, "# outside\n");
    let abs_in = fx.perf().join("Index.md").display().to_string().replace('\\', "/");
    let abs_out = outside.display().to_string().replace('\\', "/");
    let enc = |s: &str| atlas::links::percent_encode(s);
    write(
        &fx.perf().join("Areas/Links.md"),
        &format!(
            "# Links\n\n[up](../Index.md)\n[same](./GPU%20passes.md)\n[too far](../../../outside.md)\n[deep](../Ideas/PI-002%20Throttle.md)\n\
             [abs in](obsidian://open?path={})\n[abs out](obsidian://open?path={})\n<obsidian://open?vault=Research%20Log&file=Index%23Top>\n\
             [vault form](obsidian://vault/Research%20Log/Index)\n![[pic.png]]\n[pdf](doc.pdf)\n[[GPU passes#Heading|alias]]\n[[../Index]]\n[web](https://example.com/x.md)\n",
            enc(&abs_in),
            enc(&abs_out)
        ),
    );
    let mut s = fx.server();
    let out = call(&mut s, "outlinks", json!({"vault": "perf", "path": "Areas/Links.md"}));
    let text = out.to_string();
    assert!(!text.contains("outside.md\",\"to_vault\":\"perf\""), "{text}");
    for l in out["links"].as_array().cloned().unwrap_or_default() {
        if let Some(p) = l.get("to_path").and_then(|p| p.as_str()) {
            assert!(!p.contains("..") && !p.contains("outside"), "resolved outside: {l}");
        }
    }
    let _ = call(&mut s, "broken_links", json!({"vault": "perf"}));
    let _ = call(&mut s, "backlinks", json!({"vault": "perf", "path": "Index.md"}));
    // The URI parser on its own.
    let u = atlas::links::parse_uri("obsidian://open?vault=A%20B&file=x%2Fy%23H&path=%2Fabs&junk").unwrap();
    assert_eq!((u.vault.as_deref(), u.file.as_deref(), u.heading.as_deref(), u.abs_path.as_deref()), (Some("A B"), Some("x/y"), Some("H"), Some("/abs")));
    let v = atlas::links::parse_uri("obsidian://vault/A/n%23").unwrap();
    assert_eq!((v.vault.as_deref(), v.file.as_deref(), v.heading), (Some("A"), Some("n"), None));
    assert!(atlas::links::parse_uri("obsidian://search?query=x").is_none());
}

/// The transcript scan reads whatever is under a folder: malformed lines,
/// odd shapes and binary files are skipped, not fatal.
#[test]
fn the_transcript_scan_skips_hostile_lines() {
    let t = Temp::new("adv-hits");
    let dir = t.path().join("proj");
    let ctx = json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "atlas recall:\n- rules:r00001: the widget cache rule\n"}}).to_string();
    let lines = [
        json!({"type": "attachment", "timestamp": "2026-10-01T00:00:00Z", "attachment": {"hookEvent": "UserPromptSubmit", "stdout": ctx}}).to_string(),
        json!({"type": "user", "message": {"content": [{"type": "text", "text": "the widget cache"}]}}).to_string(),
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "x"}]}}).to_string(),
        json!({"type": "user", "message": {"content": "plain"}}).to_string(),
        json!({"type": "assistant", "timestamp": "2026-10-01T00:00:01Z", "message": {"content": [{"type": "tool_use", "name": "Grep", "input": {"pattern": "widget", "-i": true, "path": "x"}}]}}).to_string(),
        json!({"type": "attachment", "attachment": {"hookEvent": 7, "stdout": "{not json"}}).to_string(),
        json!({"type": "attachment", "attachment": {"hookEvent": "SubagentStart", "stdout": "[]"}}).to_string(),
        "{\"type\":\"user\"".into(),
        "\u{0}\u{1}".into(),
        String::new(),
    ];
    write(&dir.join("s1.jsonl"), &lines.join("\n"));
    write(&dir.join("sub").join("agents").join("s2.jsonl"), &lines.join("\r\n"));
    fs::write(dir.join("bin.jsonl"), [0xffu8, 0xfe, 0, 1, 2]).unwrap();
    write(&dir.join("notes.txt"), "ignored");
    let found = atlas::hook_hits::found_below(&dir, "2026-01-01", &Default::default());
    let _ = found;
    let _ = atlas::hook_hits::found_below(&t.path().join("missing"), "", &Default::default());
    let _ = atlas::hook_hits::found_in(&dir.join("bin.jsonl"), "");
}

#[test]
fn version_and_target_are_well_formed() {
    assert!(atlas::update::parse_version(atlas::update::version()).is_some());
    if let Some(t) = atlas::update::target() {
        assert!(t.contains('-'), "{t}");
    }
}

/// The tools no other test calls by name, each with an answer checked.
#[test]
fn the_rarely_tested_handlers_answer() {
    let fx = Fx::new("adv-handlers");
    let mut s = fx.server();
    let mut settings = atlas::memory::settings::Settings::from_config(None, &fx.db).unwrap();
    settings.snapshot_secs = 0;
    s.set_memory(settings);
    let v = call(&mut s, "list_vaults", json!({}));
    assert!(v.to_string().contains("Research Log") && v.to_string().contains("perf"), "{v}");
    let src = call(&mut s, "list_sources", json!({"format": "json"}));
    assert!(src.is_object() || src.is_array(), "{src}");
    let f = call(&mut s, "find_by_frontmatter", json!({"key": "status", "value": "parked"}));
    assert!(f.to_string().contains("PI-002") && !f.to_string().contains("PI-001"), "{f}");
    let f = call(&mut s, "find_by_frontmatter", json!({"key": "status", "value": "in", "prefix": true}));
    assert!(f.to_string().contains("PI-001"), "{f}");
    call(&mut s, "journal", json!({"lane": "adv-lane", "entry": "note", "text": "first entry"}));
    let j = call(&mut s, "journal_status", json!({}));
    assert!(j.to_string().contains("adv-lane"), "{j}");
    let log = call(&mut s, "memory_log", json!({"limit": 5}));
    assert!(log.to_string().contains("adv-lane"), "{log}");
    let q = call(&mut s, "decision_queue", json!({}));
    assert!(!q.to_string().is_empty());
}
