//! Semantic and hybrid search over a generic vault with a tiny random
//! model (no real weights ship with the crate): vectors are cached by
//! section text, re-embedded only when a section changes, and searched in
//! memory.

mod common;

use std::fs;

use atlas::json;
use atlas::json::Value;
use atlas::tools::{SemState, Server};
use common::*;

fn corpus(t: &Temp) -> (std::path::PathBuf, std::path::PathBuf) {
    let notes = t.path().join("Notes");
    write(&notes.join("Animals.md"), "# Animals\n\n## Cats\n\nThe cat sat on the mat.\n\n## Dogs\n\nThe dog ran fast.\n");
    write(&notes.join("Words.md"), "# Words\n\nw1 w2 w3 slow.\n\n## More\n\nw4 w5 w6.\n");
    let model = t.path().join("models/embed.atm");
    fs::create_dir_all(model.parent().unwrap()).unwrap();
    fs::write(&model, atlas::embed::tiny_model_bytes(false, 11)).unwrap();
    (notes, model)
}

fn open(t: &Temp, notes: &std::path::Path, model: &std::path::Path) -> Server {
    let db = t.path().join("state/atlas.sqlite");
    let vaults = atlas::paths::Vaults::new(&[("notes".to_string(), notes.to_path_buf())]).unwrap();
    let mut store = atlas::store::Store::open(&db, vaults).unwrap();
    store.rescan().unwrap();
    let mut s = Server::new(store);
    s.set_semantic(SemState::new(model.to_path_buf(), db, 2));
    s
}

fn paths(v: &Value) -> Vec<(String, String)> {
    v["results"].as_array().unwrap().iter().map(|h| (h["path"].as_str().unwrap().to_string(), h["heading_path"].as_str().unwrap().to_string())).collect()
}

fn cached(s: &Server) -> i64 {
    s_conn(s).query_row("SELECT COUNT(*) FROM vec_cache", [], |r| r.get(0)).unwrap()
}

fn s_conn(s: &Server) -> rusqlite::Connection {
    rusqlite::Connection::open(&s.semantic().unwrap().db).unwrap()
}

#[test]
fn semantic_search_ranks_every_section_and_caches_by_text() {
    let t = Temp::new("sem-basic");
    let (notes, model) = corpus(&t);
    let mut s = open(&t, &notes, &model);
    let v = s.call("search", &json!({"query": "the cat sat on the mat", "mode": "semantic", "limit": 10})).unwrap();
    let got = paths(&v);
    // Every section is ranked (the vault has five), the exact text first.
    assert_eq!(got.len(), 5, "{got:?}");
    assert_eq!(got[0], ("Animals.md".to_string(), "Animals > Cats".to_string()));
    assert!(v.get("vectors").is_none(), "everything embedded inline: {v}");
    let n0 = cached(&s);
    assert_eq!(n0, 5);
    // Editing one section re-embeds only that section.
    fs::write(notes.join("Animals.md"), "# Animals\n\n## Cats\n\nThe cat sat on the mat.\n\n## Dogs\n\nThe dog ran slow.\n").unwrap();
    let v = s.call("search", &json!({"query": "the dog ran slow", "mode": "semantic", "limit": 1})).unwrap();
    assert_eq!(paths(&v)[0].1, "Animals > Dogs");
    assert_eq!(cached(&s), n0 + 1, "one new vector");
    let pruned = atlas::semantic::prune(&s_conn(&s), &s.semantic().unwrap().model.as_ref().unwrap().id).unwrap();
    assert_eq!(pruned, 1, "the old Dogs vector");
    // Text answer: same shape as a sections search.
    let txt = s.answer("search", &json!({"query": "cat", "mode": "semantic", "limit": 2})).unwrap().into_text();
    assert!(txt.starts_with("loc\theading\tsnippet\n"), "{txt}");
    assert!(txt.ends_with("+more (limit=2)\n"), "{txt}");
}

#[test]
fn hybrid_fuses_words_and_vectors_and_filters_apply() {
    let t = Temp::new("sem-hybrid");
    let (notes, model) = corpus(&t);
    write(&notes.join("Sub/Other.md"), "# Other\n\nA mat for the dog.\n");
    let mut s = open(&t, &notes, &model);
    let v = s.call("search", &json!({"query": "dog mat", "mode": "hybrid", "limit": 3})).unwrap();
    let top: Vec<String> = paths(&v).into_iter().map(|p| p.0).collect();
    assert!(top.contains(&"Sub/Other.md".to_string()), "the section with both words ranks: {top:?}");
    let v = s.call("search", &json!({"query": "dog", "mode": "semantic", "folder": "Sub", "vault": "notes"})).unwrap();
    assert_eq!(paths(&v), [("Sub/Other.md".to_string(), "Other".to_string())]);
}

#[test]
fn a_missing_model_is_a_clear_error_and_sections_mode_is_unaffected() {
    let t = Temp::new("sem-missing");
    let (notes, _) = corpus(&t);
    let mut s = open(&t, &notes, &t.path().join("nope.atm"));
    let e = s.call("search", &json!({"query": "cat", "mode": "semantic"})).unwrap_err().to_string();
    assert!(e.contains("atlas --setup"), "{e}");
    assert!(s.call("search", &json!({"query": "cat", "mode": "sections"})).is_ok());
}

#[test]
fn many_missing_sections_go_to_the_background_embedder() {
    let t = Temp::new("sem-background");
    let (notes, model) = corpus(&t);
    for i in 0..(atlas::semantic::INLINE_BUDGET + 8) {
        write(&notes.join(format!("Bulk/N{i}.md")), &format!("# N{i}\n\nw{} w{} slow.\n", i % 24, (i * 7) % 24));
    }
    let mut s = open(&t, &notes, &model);
    let v = s.call("search", &json!({"query": "slow", "mode": "semantic"})).unwrap();
    assert!(v["vectors"].as_str().unwrap().starts_with("missing for "), "{v}");
    let sem = s.semantic().unwrap();
    let e = sem.embedder.as_ref().unwrap();
    assert!(e.wait(std::time::Duration::from_secs(120)));
    assert!(e.error().is_none());
    assert!(e.embedded() > atlas::semantic::INLINE_BUDGET);
    let v = s.call("search", &json!({"query": "slow", "mode": "semantic"})).unwrap();
    assert!(v.get("vectors").is_none(), "{v}");
}
