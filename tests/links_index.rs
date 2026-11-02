//! Cross-vault links and generated indices.

mod common;

use std::fs;

use common::*;
use atlas::json::{Value};
use atlas::json;
use atlas::links;
use atlas::markdown;

#[test]
fn make_link_same_and_cross_vault() {
    let fx = Fx::new("l-make");
    let mut s = fx.server();
    let r =
        call(&mut s, "make_link", json!({"from_vault": "perf", "to_vault": "perf", "path": "Ideas/PI-002 Throttle.md", "heading": "history"}));
    assert_eq!(r["link"], json!("[[PI-002 Throttle#History]]"));
    let r = call(&mut s, "make_link", json!({"from_vault": "campaign", "to_vault": "perf", "path": "Ideas/PI-002 Throttle.md"}));
    assert_eq!(r["link"], json!("[PI-002 Throttle](obsidian://open?vault=Research%20Log&file=Ideas%2FPI-002%20Throttle)"));
    let r = call(
        &mut s,
        "make_link",
        json!({"from_vault": "campaign", "to_vault": "perf", "path": "Ideas/PI-002 Throttle.md", "heading": "History"}),
    );
    assert_eq!(
        r["link"],
        json!("[PI-002 Throttle > History](obsidian://open?vault=Research%20Log&file=Ideas%2FPI-002%20Throttle%23History)")
    );
    let e = call_err(&mut s, "make_link", json!({"from_vault": "campaign", "to_vault": "perf", "path": "Ideas/PI-404 Nope.md"}));
    assert!(e.contains("not found"), "{e}");
    let e = call_err(&mut s, "make_link", json!({"from_vault": "campaign", "to_vault": "perf", "path": "Index.md", "heading": "Nope"}));
    assert!(e.contains("not found"), "{e}");
}

#[test]
fn vault_name_is_the_folder_name_not_an_assumed_one() {
    let tmp = Temp::new("l-rename");
    let specs = fixture(tmp.path());
    // The perf vault lives in a folder with another name.
    let renamed = tmp.path().join("Perf Copy");
    fs::rename(&specs[0].1, &renamed).unwrap();
    let specs = vec![("perf".to_string(), renamed), specs[1].clone(), specs[2].clone()];
    let mut s = server_with(&specs, &tmp.path().join("db.sqlite"));
    let r = call(&mut s, "make_link", json!({"from_vault": "campaign", "to_vault": "perf", "path": "Index.md"}));
    assert_eq!(r["link"], json!("[Index](obsidian://open?vault=Perf%20Copy&file=Index)"));
    // The campaign note links by the old folder name, so it is now broken.
    let r = call(&mut s, "broken_links", json!({"vault": "campaign"}));
    let raws: Vec<&str> = r["links"].as_array().unwrap().iter().map(|l| l["raw"].as_str().unwrap()).collect();
    assert!(raws.iter().any(|x| x.contains("vault=Research%20Log")), "{raws:?}");
    let r = call(&mut s, "list_vaults", json!({}));
    assert_eq!(r["vaults"][0]["folder"], json!("Perf Copy"));
}

#[test]
fn backlinks_outlinks_and_broken_across_vaults() {
    let fx = Fx::new("l-graph");
    let mut s = fx.server();
    let r = call(&mut s, "outlinks", json!({"vault": "campaign", "path": "Missions/M1 Hold.md"}));
    let l = r["links"].as_array().unwrap();
    assert_eq!(l.len(), 2);
    assert_eq!(l[0]["kind"], json!("uri"));
    assert_eq!(l[0]["to_vault"], json!("perf"));
    assert_eq!(l[0]["to_path"], json!("Ideas/PI-002 Throttle.md"));
    assert_eq!(l[1]["to_path"], Value::Null);
    let r = call(&mut s, "broken_links", json!({}));
    let mut got: Vec<String> = r["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| format!("{}:{}", l["from_vault"].as_str().unwrap(), l["target"].as_str().unwrap()))
        .collect();
    got.sort();
    // [[PACKAGE]] sits in a code fence and is not a link.
    assert_eq!(got, vec!["campaign:Nowhere", "perf:LOADER-WORK"]);
    // Creating the missing note fixes the link without touching the linker.
    call(&mut s, "create_note", json!({"vault": "campaign", "path": "Nowhere.md", "content": "# here\n"}));
    let r = call(&mut s, "broken_links", json!({"vault": "campaign"}));
    assert_eq!(r["total"], json!(0));
    let r = call(&mut s, "backlinks", json!({"vault": "campaign", "path": "Nowhere.md"}));
    assert_eq!(r["links"][0]["from_path"], json!("Missions/M1 Hold.md"));
}

#[test]
fn link_notes_appends_under_related_and_is_idempotent() {
    let fx = Fx::new("l-linknotes");
    let mut s = fx.server();
    let args = json!({"from_vault": "campaign", "from_path": "Missions/M1 Hold.md", "to_vault": "perf", "to_path": "Ideas/PI-001 Cache inside the loader.md", "reverse": true});
    call(&mut s, "link_notes", args.clone());
    let m1 = fs::read_to_string(fx.camp().join("Missions/M1 Hold.md")).unwrap();
    assert!(m1.ends_with("\n## Related\n\n- [PI-001 Cache inside the loader](obsidian://open?vault=Research%20Log&file=Ideas%2FPI-001%20Cache%20inside%20the%20loader)\n"), "{m1}");
    let p1 = fs::read_to_string(fx.perf().join("Ideas/PI-001 Cache inside the loader.md")).unwrap();
    assert!(p1.contains("## Related\n\n- [[PI-002 Throttle]]\n- [M1 Hold](obsidian://open?vault=Field%20Notes&file=Missions%2FM1%20Hold)\n\n## History"), "{p1}");
    let r = call(&mut s, "link_notes", args);
    assert_eq!(r["forward"]["changed"], json!(false));
    assert_eq!(r["reverse"]["changed"], json!(false));
    let r = call(&mut s, "backlinks", json!({"vault": "perf", "path": "Ideas/PI-001 Cache inside the loader.md"}));
    assert_eq!(r["links"][0]["from_vault"], json!("campaign"));
}

/// Resolves a generated link the way Obsidian will when it is clicked.
fn open(link: &str) -> (Option<String>, Option<String>) {
    let start = link.find("](").unwrap() + 2;
    let uri = &link[start..link.len() - 1];
    let u = links::parse_uri(uri).unwrap();
    (u.vault, u.file)
}

#[test]
fn indices_keep_outside_text_refresh_idempotently_and_link_across_vaults() {
    let fx = Fx::new("l-index");
    let mut s = fx.server();
    let hand = "# Everything parked\n\nHand-written intro, kept.\n\n";
    write(&fx.camp().join("Parked.md"), &format!("{hand}## Tail\n\nAlso kept.\n"));
    let spec = json!({
        "sources": [{"vault": "perf", "folder": "Ideas", "glob": "PI-*.md"}],
        "filter": [{"key": "status", "value": "parked"}],
        "sort": "id",
        "columns": ["id", "link", "status"]
    });
    let r = call(&mut s, "build_index", json!({"vault": "campaign", "path": "Parked.md", "name": "parked", "spec": spec}));
    assert_eq!(r["changed"], json!(true));
    let text = fs::read_to_string(fx.camp().join("Parked.md")).unwrap();
    assert!(text.starts_with(&format!("{hand}## Tail\n\nAlso kept.\n\n<!-- vault-mcp:index parked start {{")), "{text}");
    assert!(text.ends_with("<!-- vault-mcp:index parked end -->\n"));
    // The cross-vault row opens the right note.
    let row = text.lines().find(|l| l.starts_with("| PI-002")).unwrap();
    let cell = row.split(" | ").nth(1).unwrap();
    let (vault, file) = open(cell);
    assert_eq!(vault.as_deref(), Some("Research Log"));
    assert_eq!(file.as_deref(), Some("Ideas/PI-002 Throttle"));
    assert!(fx.perf().join(format!("{}.md", file.unwrap())).is_file());
    let r = call(&mut s, "outlinks", json!({"vault": "campaign", "path": "Parked.md"}));
    assert_eq!(r["links"][0]["to_path"], json!("Ideas/PI-002 Throttle.md"));

    // Refresh with nothing changed: byte-identical and no write.
    let writes_before: i64 = s.store_mut().conn().query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0)).unwrap();
    let r = call(&mut s, "refresh_index", json!({"vault": "campaign", "path": "Parked.md"}));
    assert_eq!(r["changed"], json!(false));
    let r = call(&mut s, "refresh_all_indices", json!({}));
    assert_eq!(r["results"][0]["changed"], json!(false));
    assert_eq!(fs::read_to_string(fx.camp().join("Parked.md")).unwrap(), text);
    let writes_after: i64 = s.store_mut().conn().query_row("SELECT COUNT(*) FROM writes", [], |r| r.get(0)).unwrap();
    assert_eq!(writes_before, writes_after);

    // Edit outside the markers by hand, park another idea, refresh: the hand edit survives.
    let edited = text.replace("Also kept.", "Also kept, and edited by hand.");
    fs::write(fx.camp().join("Parked.md"), &edited).unwrap();
    call(
        &mut s,
        "set_frontmatter",
        json!({"vault": "perf", "path": "Ideas/PI-001 Cache inside the loader.md", "values": {"status": "parked"}}),
    );
    let r = call(&mut s, "refresh_all_indices", json!({}));
    assert_eq!(r["results"][0]["changed"], json!(true));
    let now = fs::read_to_string(fx.camp().join("Parked.md")).unwrap();
    let start = now.find("<!-- vault-mcp:index").unwrap();
    assert_eq!(&now[..start], &edited[..edited.find("<!-- vault-mcp:index").unwrap()]);
    assert!(now.contains("| PI-001 |") && now.contains("| PI-002 |"));
}

#[test]
fn two_named_indices_in_one_note_are_independent() {
    let fx = Fx::new("l-two");
    let mut s = fx.server();
    let open_spec = json!({"sources": [{"vault": "perf", "folder": "Ideas"}], "filter": [{"key": "status", "value": "in flight", "prefix": true}], "format": "list"});
    let parked_spec =
        json!({"sources": [{"vault": "perf", "folder": "Ideas"}], "filter": [{"key": "status", "value": "parked"}], "format": "list"});
    call(&mut s, "build_index", json!({"vault": "perf", "path": "Two.md", "name": "flight", "spec": open_spec}));
    call(&mut s, "append_to_note", json!({"vault": "perf", "path": "Two.md", "text": "Between the two."}));
    call(&mut s, "build_index", json!({"vault": "perf", "path": "Two.md", "name": "parked", "spec": parked_spec}));
    let t = fs::read_to_string(fx.perf().join("Two.md")).unwrap();
    assert!(t.contains("start {") && t.contains("- [[PI-001 Cache inside the loader]]\n<!-- vault-mcp:index flight end -->\nBetween the two.\n\n<!-- vault-mcp:index parked start"), "{t}");
    // Changing one idea and refreshing only `parked` leaves `flight` alone even though it is now stale.
    call(&mut s, "set_frontmatter", json!({"vault": "perf", "path": "Ideas/PI-002 Throttle.md", "values": {"status": "in flight X"}}));
    call(&mut s, "refresh_index", json!({"vault": "perf", "path": "Two.md", "name": "parked"}));
    let u = fs::read_to_string(fx.perf().join("Two.md")).unwrap();
    let flight = |x: &str| x[..x.find("flight end").unwrap()].to_string();
    assert_eq!(flight(&u), flight(&t));
    assert!(u.contains("<!-- vault-mcp:index parked start") && u.contains("_No notes match._\n<!-- vault-mcp:index parked end -->"));
    call(&mut s, "refresh_index", json!({"vault": "perf", "path": "Two.md", "name": "flight"}));
    let w = fs::read_to_string(fx.perf().join("Two.md")).unwrap();
    assert!(flight(&w).contains("PI-002 Throttle"));
    assert_eq!(markdown::headings(&w).len(), 0);
}

#[test]
fn next_id_takes_the_next_free_number() {
    let fx = Fx::new("l-nextid");
    let mut s = fx.server();
    let r = call(
        &mut s,
        "next_id",
        json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-", "name": "Third idea", "content": "---\nid: {{id}}\n---\n# {{id}} Third idea\n"}),
    );
    assert_eq!(r["id"], json!("PI-003"));
    assert_eq!(r["note"]["path"], json!("Ideas/PI-003 Third idea.md"));
    assert_eq!(fs::read_to_string(fx.perf().join("Ideas/PI-003 Third idea.md")).unwrap(), "---\nid: PI-003\n---\n# PI-003 Third idea\n");
    // A reservation without a note still consumes the number.
    assert_eq!(call(&mut s, "next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-"}))["id"], json!("PI-004"));
    // A file made by hand outside the server is skipped.
    write(&fx.perf().join("Ideas/PI-005 By hand.md"), "x");
    assert_eq!(call(&mut s, "next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-"}))["id"], json!("PI-006"));
    let e = call_err(&mut s, "next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-", "name": "x"}));
    assert!(e.contains("both"), "{e}");
}

#[test]
fn next_id_is_unique_across_folders() {
    let fx = Fx::new("l-nextid-vault");
    let mut s = fx.server();
    // An id taken in another folder of the same vault is not handed out again.
    write(&fx.perf().join("Other/PI-007 Elsewhere.md"), "x");
    assert_eq!(call(&mut s, "next_id", json!({"vault": "perf", "folder": "Ideas", "prefix": "PI-"}))["id"], json!("PI-008"));
    assert_eq!(call(&mut s, "next_id", json!({"vault": "perf", "folder": "Other", "prefix": "PI-"}))["id"], json!("PI-009"));
}

/// The README's index specs, run on a copy of a real ideas vault when
/// `ATLAS_LIVE_IDEAS_VAULT` names one (a folder holding `Ideas/PI-*.md`
/// notes with `status` frontmatter). Read-only on that vault; skipped when the
/// variable is unset.
#[test]
fn readme_index_specs_render_on_a_copy_of_a_live_vault() {
    let Some(live) = std::env::var_os("ATLAS_LIVE_IDEAS_VAULT").map(std::path::PathBuf::from) else {
        eprintln!("ATLAS_LIVE_IDEAS_VAULT not set; skipped");
        return;
    };
    let tmp = Temp::new("l-live");
    let copy = tmp.path().join("Live copy");
    copy_md(&live, &copy);
    let specs = vec![("perf".to_string(), copy.clone())];
    let mut s = server_with(&specs, &tmp.path().join("db.sqlite"));
    // Build into fresh notes, so the test does not depend on whether the live Index.md is hand-written or generated.
    let ideas = fs::read_dir(copy.join("Ideas")).unwrap().count();
    let order = json!(["open", "queued", "in flight", "done", "parked"]);
    let src = json!([{"vault": "perf", "folder": "Ideas", "glob": "PI-*.md"}]);
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "Index check.md", "name": "counts", "spec": {
        "sources": src, "group_by": {"key": "status", "order": order}, "format": "counts", "count_header": "Ideas"}}),
    );
    call(
        &mut s,
        "build_index",
        json!({"vault": "perf", "path": "Index check.md", "name": "all-ideas", "spec": {
        "sources": src, "sort": "id", "link_text": "title",
        "columns": [{"key": "id", "header": "ID"}, {"key": "link", "header": "Idea"}, {"key": "status", "header": "Status"}]}}),
    );
    let t = fs::read_to_string(copy.join("Index check.md")).unwrap();
    assert_eq!(t.lines().filter(|l| l.starts_with("| PI-")).count(), ideas, "one generated row per idea");
    assert_eq!(t.matches(&format!("| total | {ideas} |")).count(), 1, "the counts table totals every idea");
    let r = call(&mut s, "refresh_index", json!({"vault": "perf", "path": "Index check.md"}));
    assert_eq!(r["changed"], json!(false));
}

fn copy_md(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let name = e.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let ft = e.file_type().unwrap();
        if ft.is_dir() {
            copy_md(&e.path(), &to.join(&name));
        } else if ft.is_file() {
            fs::copy(e.path(), to.join(&name)).unwrap();
        }
    }
}
