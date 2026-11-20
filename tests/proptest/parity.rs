//! The worktree scan and the per-edit re-index read a file the same way: for
//! generated Rust sources, a linked worktree's copy gets the items the
//! checkout's copy has after `reindex_file`.
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use atlas::code::{self, CodeSetup};
use atlas::code_index::{self, RustSource};
use proptest::prelude::*;

struct Lab {
    db: PathBuf,
    root: PathBuf,
    wt: PathBuf,
}

/// A root that is a repository's checkout, with one linked worktree beside
/// it, indexed once (the folder is left in the temp folder).
fn lab() -> &'static Lab {
    static LAB: OnceLock<Lab> = OnceLock::new();
    LAB.get_or_init(|| {
        let t = std::env::temp_dir().join(format!("atlas-proptest-parity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&t);
        let (root, wt, db) = (t.join("app"), t.join("app-wt"), t.join("state/atlas.sqlite"));
        let admin = root.join(".git/worktrees/wt");
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(wt.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn seed() {}\n").unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(admin.join("gitdir"), format!("{}\n", wt.join(".git").display())).unwrap();
        let notes = t.join("state/notes");
        fs::create_dir_all(&notes).unwrap();
        let vaults = atlas::paths::Vaults::new(&[("notes".to_string(), notes)]).unwrap();
        drop(atlas::store::Store::open(&db, vaults).unwrap());
        let spec = atlas::config::CodeSpec { roots: vec![atlas::config::CodeRootSpec { name: "app".into(), root: root.clone(), build_scripts: false, scip: Default::default(), exclude: Vec::new() }], debounce_s: Some(0), ..Default::default() };
        let code = CodeSetup::from_spec(&spec, &db, false);
        let mut conn = atlas::db::Connection::open(&db).unwrap();
        code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::ScanOnly("proptest".into())).unwrap();
        Lab { db, root, wt }
    })
}

fn ty(n: &str) -> String {
    let mut c = n.chars();
    c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
}

fn item() -> impl Strategy<Value = String> {
    let name = "[a-z][a-z0-9_]{0,6}";
    prop_oneof![
        name.prop_map(|n| format!("pub fn {n}() {{}}\n")),
        (name, 0usize..4).prop_map(|(n, k)| format!("fn {n}(x: u32) -> u32 {{\n{}    x\n}}\n", "    let _y = x;\n".repeat(k))),
        name.prop_map(|n| format!("pub struct {} {{\n    a: u32,\n}}\n", ty(&n))),
        (name, name).prop_map(|(t, m)| format!("impl {} {{\n    pub fn {m}(&self) -> u32 {{\n        1\n    }}\n}}\n", ty(&t))),
        (name, name).prop_map(|(t, m)| format!("impl std::fmt::Display for {} {{\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {{ write!(f, \"{m} {{}}\") }}\n}}\n", ty(&t))),
        name.prop_map(|n| format!("// fn {n}() in a comment {{\n")),
        name.prop_map(|n| format!("const S_{}: &str = \"fn {n}() {{\";\n", n.to_uppercase())),
        (name, name).prop_map(|(m, f)| format!("mod {m} {{\n    fn {f}() {{}}\n}}\n")),
        name.prop_map(|n| format!("macro_rules! {n} {{\n    () => {{}};\n}}\n")),
        Just("\n".to_string()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn a_worktree_copy_reads_like_the_reindexed_checkout(src in prop::collection::vec(item(), 1..10).prop_map(|v| v.concat())) {
        let l = lab();
        let (main, copy) = (l.root.join("src/gen.rs"), l.wt.join("src/gen.rs"));
        fs::write(&main, &src).unwrap();
        fs::write(&copy, &src).unwrap();
        let mut conn = atlas::db::Connection::open(&l.db).unwrap();
        code::reindex_file(&mut conn, &main.to_string_lossy()).unwrap();
        let from_rows = code::file_items(&conn, &main.to_string_lossy()).unwrap();
        let from_scan = code::file_items(&conn, &copy.to_string_lossy()).unwrap();
        prop_assert!(from_rows.is_some(), "the checkout's copy is not covered:\n{}", src);
        prop_assert_eq!(from_scan, from_rows, "{}", src);
    }
}
