//! The per-edit re-index takes its stamp before it reads the file, so an edit
//! landing during the read is not taken as indexed. A named pipe is the seam
//! between the two: opening it to write blocks until `reindex_file` opens it
//! to read, which it does after its stamp, so the write (and the pipe's mtime)
//! comes after the stamp. With the stamp taken after the read, the file would
//! pass as indexed. Linux only: there a write to a pipe sets its mtime.
#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::io::Write;
use std::process::Command;
use std::time::Duration;

use atlas::code::{self, CodeSetup};
use atlas::code_index::{self, RustSource};
use common::*;

#[test]
fn a_file_written_during_the_reindex_read_is_not_taken_as_indexed() {
    let t = Temp::new("reindex-fifo");
    let root = t.path().join("app");
    write(&root.join("src/lib.rs"), "pub fn lib() {}\n");
    let db = t.path().join("state/atlas.sqlite");
    let notes = t.path().join("state/notes");
    fs::create_dir_all(&notes).unwrap();
    drop(atlas::store::Store::open(&db, atlas::paths::Vaults::new(&[("notes".to_string(), notes)]).unwrap()).unwrap());
    let spec = atlas::config::CodeSpec {
        roots: vec![atlas::config::CodeRootSpec {
            name: "app".into(),
            root: root.clone(),
            build_scripts: false,
            scip: Default::default(),
            exclude: Vec::new(),
        }],
        debounce_s: Some(0),
        ..Default::default()
    };
    let code = CodeSetup::from_spec(&spec, &db, false);
    code_index::index_root(&code, &mut atlas::db::Connection::open(&db).unwrap(), &code.roots[0], &RustSource::ScanOnly("test".into()))
        .unwrap();
    let pipe = root.join("src/pipe.rs");
    assert!(Command::new("mkfifo").arg(&pipe).status().unwrap().success());
    let (db2, pipe2) = (db.clone(), pipe.to_string_lossy().into_owned());
    let reader = std::thread::spawn(move || code::reindex_file(&mut atlas::db::Connection::open(&db2).unwrap(), &pipe2).unwrap());
    // Blocks until the re-index opens the pipe to read, after its stamp.
    let mut w = fs::OpenOptions::new().write(true).open(&pipe).unwrap();
    // Past the stamp's millisecond: the compare is a strict `>`.
    std::thread::sleep(Duration::from_millis(5));
    w.write_all(b"pub fn piped() {}\n").unwrap();
    drop(w);
    assert!(matches!(reader.join().unwrap(), code::Reindexed::Rows(_)));
    let conn = atlas::db::Connection::open(&db).unwrap();
    assert_eq!(code::file_items(&conn, &pipe.to_string_lossy()).unwrap(), None, "written after the stamp, so not served as indexed");
}
