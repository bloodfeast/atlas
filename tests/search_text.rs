//! What the docs say about search scope and scripts: substring search reads
//! vault notes and finds text written without spaces, and an empty answer
//! says where the sources are searched.

mod common;

use atlas::json;
use common::*;

#[test]
fn substring_search_finds_text_without_spaces_and_names_its_scope() {
    let fx = Fx::new("search-cjk");
    write(&fx.perf().join("Tokyo.md"), "# Tokyo\n\n東京タワーの高さは333メートル\nlaunch🚀day\n");
    let mut s = fx.server();
    for q in ["タワー", "day"] {
        let out = call(&mut s, "search", json!({"query": q})).to_string();
        assert!(out.contains("Tokyo.md"), "{q}: {out}");
    }
    // A vault-only miss with no sources configured says nothing extra.
    let out = call(&mut s, "search", json!({"query": "nowhere-at-all"})).to_string();
    assert!(!out.contains("sources:"), "{out}");
}

#[test]
fn an_empty_vault_search_names_where_sources_are_searched() {
    let t = Temp::new("search-scope");
    write(&t.path().join("vault").join("a.md"), "# A\n\nalpha\n");
    write(&t.path().join("src").join("notes.md"), "# Notes\n\nSPH kernel\n");
    let cfg = t.path().join("atlas.json");
    write(&cfg, r#"{"db": "state/atlas.sqlite", "vaults": [{"name": "v", "path": "vault"}], "sources": [{"name": "s", "root": "src", "include": ["*.md"]}], "models": {"auto_download": false, "files": []}}"#);
    let out = std::process::Command::new(bin())
        .arg("--config")
        .arg(&cfg)
        .current_dir(t.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            let mut i = c.stdin.take().unwrap();
            writeln!(i, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"clientInfo": {"name": "t"}}}))?;
            writeln!(i, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "search", "arguments": {"query": "SPH"}}}))?;
            drop(i);
            c.wait_with_output()
        })
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("this mode reads vaults only; sources: mode sections, or grep"), "{text}");
}
