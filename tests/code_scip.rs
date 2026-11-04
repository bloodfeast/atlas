//! The code index over SCIP files from indexers other than rust-analyzer,
//! through `--code-index --scip FILE` ([`RustSource::File`]).
//!
//! Each fixture under `fixtures/scip` is a tiny project written for atlas
//! with the `index.scip` its indexer wrote, checked in so the tests need no
//! indexer:
//!
//! | Fixture | Indexer that wrote `index.scip` | Ranges | Kinds | Enclosing ranges |
//! |---|---|---|---|---|
//! | `go` | scip-go 0.2.9 (go1.26.0) | typed only (fields 8 to 11) | yes | yes |
//! | `ts` | scip-typescript 0.5.2 (Node 22) | packed (fields 1 and 7) | yes | yes |
//! | `cs` | scip-dotnet 0.2.16, metadata `0.1.0-SNAPSHOT` (.NET SDK 8.0.427) | packed | none (all 0) | none |
//! | `py` | scip-python 0.6.8 (its Docker image) | packed | none (all 0) | yes |
//!
//! To regenerate one, copy its folder somewhere outside any user path (the
//! file records `project_root`) and run, in the copy, `scip-go --output
//! index.scip`, `scip-typescript index --output index.scip` or
//! `scip-dotnet index Shapes.csproj --output index.scip` (for `py`, in
//! scip-python's Docker image, which does not run on Windows:
//! `scip-python index . --project-name shapes --project-version 0.1.0
//! --output index.scip`); then copy
//! `index.scip` back and update the versions here and in the tests.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use atlas::code::{self, CodeSetup, Unit};
use atlas::code_index::{self, IndexReport, RustSource};
use atlas::json;
use atlas::json::Value;
use atlas::protobuf::{Reader, WireType};
use atlas::scip;
use atlas::tools::{CodeState, Server};
use common::*;

fn fixture_dir(lang: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("scip").join(lang)
}

/// A copy of the fixture project (a fresh folder, so files are older than
/// the index written next), its database and its `index.scip`.
struct Fixture {
    _t: Temp,
    root: PathBuf,
    db: PathBuf,
    scip: PathBuf,
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

fn fixture(lang: &str, extra: &[(&str, &str)]) -> Fixture {
    let t = Temp::new(&format!("code-scip-{lang}"));
    let root = t.path().join(lang);
    copy_dir(&fixture_dir(lang), &root);
    let scip = t.path().join("index.scip");
    fs::rename(root.join("index.scip"), &scip).unwrap();
    for (p, text) in extra {
        write(&root.join(p), text);
    }
    let db = t.path().join("state/atlas.sqlite");
    Fixture { root, db, scip, _t: t }
}

fn vaults(db: &Path) -> atlas::paths::Vaults {
    let d = db.parent().unwrap().join("notes");
    fs::create_dir_all(&d).unwrap();
    atlas::paths::Vaults::new(&[("notes".to_string(), d)]).unwrap()
}

fn setup(f: &Fixture, name: &str) -> CodeSetup {
    let spec = atlas::config::CodeSpec { roots: vec![atlas::config::CodeRootSpec { name: name.into(), root: f.root.clone(), build_scripts: false, scip: Default::default() }], debounce_s: Some(0), ..Default::default() };
    let mut code = CodeSetup::from_spec(&spec, &f.db, false);
    code.defer_on_cargo = false;
    code
}

/// Indexes the fixture from its SCIP file and opens a server over it.
fn indexed(f: &Fixture, name: &str) -> (IndexReport, Server) {
    let code = setup(f, name);
    drop(atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap());
    let mut conn = rusqlite::Connection::open(&f.db).unwrap();
    let rep = code_index::index_root(&code, &mut conn, &code.roots[0], &RustSource::File(f.scip.clone())).unwrap();
    let store = atlas::store::Store::open(&f.db, vaults(&f.db)).unwrap();
    let mut s = Server::new(store);
    s.set_code(CodeState { setup: code, indexer: None });
    (rep, s)
}

/// `path:line kind container` for definitions, `path:line within` for
/// references, `line:container` for impl edges.
fn answer(s: &mut Server, name: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let v = s.call("symbol", &json!({ "name": name })).unwrap();
    let list = |key: &str, f: &dyn Fn(&Value) -> String| -> Vec<String> { v[key].as_array().unwrap().iter().map(f).collect() };
    (
        list("defs", &|h| format!("{}:{} {} {}", h["path"].as_str().unwrap(), h["line"], h["kind"].as_str().unwrap(), h["container"].as_str().unwrap())),
        list("refs", &|h| format!("{}:{} {}", h["path"].as_str().unwrap(), h["line"], h["within"].as_str().unwrap())),
        list("impls", &|h| format!("{}:{}", h["line"], h["container"].as_str().unwrap())),
    )
}

/// Counts of occurrences by range form: (packed range, typed range, packed
/// enclosing range, typed enclosing range).
fn range_forms(buf: &[u8]) -> [usize; 4] {
    let mut n = [0; 4];
    let mut r = Reader::new(buf);
    while let Some((f, wt)) = r.field().unwrap() {
        if (f, wt) != (2, WireType::Len) {
            r.skip(wt).unwrap();
            continue;
        }
        let mut d = Reader::new(r.bytes().unwrap());
        while let Some((g, wt2)) = d.field().unwrap() {
            if (g, wt2) != (2, WireType::Len) {
                d.skip(wt2).unwrap();
                continue;
            }
            let mut o = Reader::new(d.bytes().unwrap());
            while let Some((h, wt3)) = o.field().unwrap() {
                match h {
                    1 => n[0] += 1,
                    8 | 9 => n[1] += 1,
                    7 => n[2] += 1,
                    10 | 11 => n[3] += 1,
                    _ => {}
                }
                o.skip(wt3).unwrap();
            }
        }
    }
    n
}

#[test]
fn fixture_indexes_decode_in_both_range_forms() {
    for (lang, tool, forms_ok) in [
        ("go", ("scip-go", "0.2.9"), (|n: [usize; 4]| n[0] == 0 && n[2] == 0 && n[1] > 0 && n[3] > 0) as fn([usize; 4]) -> bool),
        ("ts", ("scip-typescript", "0.5.2"), |n| n[1] == 0 && n[3] == 0 && n[0] > 0 && n[2] > 0),
        ("cs", ("scip-dotnet", "0.1.0-SNAPSHOT"), |n| n[1] == 0 && n[3] == 0 && n[0] > 0 && n[2] == 0),
        ("py", ("scip-python", "0.6.8"), |n| n[1] == 0 && n[3] == 0 && n[0] > 0 && n[2] > 0),
        ("java", ("scip-java", "HEAD-20260402-1750"), |n| n[1] == 0 && n[3] == 0 && n[0] > 0 && n[2] > 0),
    ] {
        let buf = fs::read(fixture_dir(lang).join("index.scip")).unwrap();
        let n = range_forms(&buf);
        assert!(forms_ok(n), "{lang}: range forms {n:?}");
        let ix = scip::decode(&buf, scip::Options::default()).unwrap();
        assert_eq!(ix.tool, tool, "{lang}");
        assert!(!code::is_rust_index(&ix));
        let defs = (0..ix.occurrences()).filter(|&i| ix.occ_roles[i] & scip::ROLE_DEFINITION != 0).count();
        assert!(defs > 0, "{lang}");
        let kinds = ix.info.iter().filter(|i| i.known && i.kind != 0).count();
        assert_eq!(kinds == 0, matches!(lang, "cs" | "py"), "{lang}: scip-dotnet and scip-python give no kinds ({kinds})");
        let rels = (0..ix.rel_from.len()).filter(|&r| ix.rel_flags[r] & scip::REL_IMPLEMENTATION != 0).count();
        assert!(rels >= 2, "{lang}: implementation relationships {rels}");
    }
}

#[test]
fn go_typed_ranges_give_definitions_callers_and_impl_edges() {
    let f = fixture("go", &[]);
    let (rep, mut s) = indexed(&f, "shapes");
    assert_eq!(rep.units, [Unit { dir: String::new(), lang: "go".into(), source: "prebuilt scip-go 0.2.9".into() }]);
    assert_eq!(rep.rust, "go: prebuilt scip-go 0.2.9");
    let (defs, refs, impls) = answer(&mut s, "Square");
    assert_eq!(defs, ["shapes.go:10 struct shapes"]);
    assert!(refs.contains(&"scale.go:4 shapes::Double".to_string()), "{refs:?}");
    assert!(refs.contains(&"shapes.go:15 Square::Area".to_string()), "{refs:?}");
    assert_eq!(impls, ["10:Shape for Square"]);
    let (defs, refs, impls) = answer(&mut s, "Area");
    assert_eq!(defs, ["shapes.go:6 method Shape", "shapes.go:15 method Square"]);
    assert_eq!(refs, ["shapes.go:23 shapes::Total"]);
    assert_eq!(impls, ["15:Shape::Area for Square::Area"], "a member implementing its namesake is one row");
    let (defs, _, _) = answer(&mut s, "Square::Area");
    assert_eq!(defs, ["shapes.go:15 method Square"]);
    let (defs, _, _) = answer(&mut s, "Unit");
    assert_eq!(defs, ["shapes.go:29 const shapes"]);
    // End lines from the brace matcher.
    let t = s.answer("symbol", &json!({"name": "Total"})).unwrap().into_text();
    assert!(t.contains("shapes:shapes.go:20-26\tfn\tshapes\tfunc Total(shapes []Shape) float64 {"), "{t}");
    // Read and Grep annotations: the method is the item, not its impl edge.
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let items = code::file_items(&conn, &f.root.join("shapes.go").to_string_lossy()).unwrap().unwrap();
    let labels: Vec<String> = items.iter().map(|i| i.label()).collect();
    assert_eq!(labels, ["interface shapes::Shape 5-7", "fn Shape::Area 6-6", "struct shapes::Square 10-12", "field Square::Side 11-11", "fn Square::Area 15-17", "fn shapes::Total 20-26", "const shapes::Unit 29-29"]);
    assert_eq!(code::enclosing(&items, 16).map(|i| i.name.as_str()), Some("Square::Area"));
}

#[test]
fn typescript_packed_ranges_constructors_and_files() {
    let f = fixture("ts", &[]);
    let (rep, mut s) = indexed(&f, "web");
    assert_eq!(rep.rust, "typescript: prebuilt scip-typescript 0.5.2");
    let (defs, refs, impls) = answer(&mut s, "Square");
    assert_eq!(defs, ["src/shapes.ts:7 class shapes", "src/shapes.ts:8 ctor Square"]);
    assert!(refs.contains(&"src/main.ts:6 main::report".to_string()), "{refs:?}");
    assert_eq!(impls, ["7:Shape for Square"]);
    let (defs, refs, impls) = answer(&mut s, "area");
    assert_eq!(defs, ["src/shapes.ts:3 method Shape", "src/shapes.ts:10 method Square"]);
    assert_eq!(refs, ["src/shapes.ts:19 shapes::total"]);
    assert_eq!(impls, ["10:Shape::area for Square::area"]);
    let (defs, refs, _) = answer(&mut s, "UNIT");
    assert_eq!(defs, ["src/main.ts:3 const main"]);
    assert_eq!(refs, ["src/main.ts:6 main::report"]);
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let lang: String = conn.query_row("SELECT lang FROM code_files WHERE path = 'src/main.ts'", [], |r| r.get(0)).unwrap();
    assert_eq!(lang, "typescript");
}

#[test]
fn csharp_without_kinds_or_ranges_uses_the_line_and_the_brace_matcher() {
    let f = fixture("cs", &[]);
    let (_, mut s) = indexed(&f, "svc");
    let (defs, _, impls) = answer(&mut s, "Square");
    assert_eq!(defs, ["Shapes.cs:10 class Shapes", "Shapes.cs:12 ctor Square"]);
    assert_eq!(impls, ["10:IShape for Square"]);
    let (defs, _, _) = answer(&mut s, "IShape");
    assert_eq!(defs, ["Shapes.cs:4 interface Shapes"]);
    let (defs, refs, _) = answer(&mut s, "Side");
    assert_eq!(defs, ["Shapes.cs:17 property Square"]);
    // Callers from the matcher's ranges: scip-dotnet writes no enclosing ranges.
    assert_eq!(refs, ["Shapes.cs:14 Square::Square", "Shapes.cs:21 Square::Area", "Shapes.cs:21 Square::Area"]);
    let (defs, refs, impls) = answer(&mut s, "Area");
    assert_eq!(defs, ["Shapes.cs:6 method IShape", "Shapes.cs:19 method Square"]);
    assert_eq!(refs, ["Shapes.cs:32 Sums::Total"]);
    assert_eq!(impls, ["19:IShape::Area for Square::Area"]);
    let t = s.answer("symbol", &json!({"name": "Sums"})).unwrap().into_text();
    assert!(t.contains("svc:Shapes.cs:25-36\tclass\tShapes\t"), "{t}");
}

#[test]
fn a_root_with_rust_beside_another_language_has_two_units() {
    let f = fixture("go", &[("tool/src/lib.rs", "pub fn helper() -> u32 { 1 }\n")]);
    let (rep, mut s) = indexed(&f, "mixed");
    assert_eq!(
        rep.units.iter().map(|u| (u.lang.as_str(), u.source.as_str())).collect::<Vec<_>>(),
        [("rust", "scan only: the SCIP file is from scip-go"), ("go", "prebuilt scip-go 0.2.9")]
    );
    assert_eq!(rep.rust, "rust: scan only: the SCIP file is from scip-go; go: prebuilt scip-go 0.2.9");
    let (defs, _, _) = answer(&mut s, "helper");
    assert_eq!(defs, ["tool/src/lib.rs:1 fn (scan) "]);
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let units: i64 = conn.query_row("SELECT COUNT(*) FROM code_units WHERE root = 'mixed'", [], |r| r.get(0)).unwrap();
    assert_eq!(units, 2);
}

#[test]
fn python_without_kinds_uses_the_line_and_the_indentation_blocks() {
    let f = fixture("py", &[]);
    let (rep, mut s) = indexed(&f, "shapes");
    assert_eq!(rep.rust, "python: prebuilt scip-python 0.6.8");
    let (defs, refs, impls) = answer(&mut s, "Square");
    assert_eq!(defs, ["shapes/geometry.py:37 class geometry", "shapes/geometry.py:40 ctor Square"]);
    assert!(refs.contains(&"shapes/main.py:5 main::main".to_string()), "{refs:?}");
    assert_eq!(impls, ["37:Shape for Square"]);
    let (defs, refs, impls) = answer(&mut s, "area");
    assert_eq!(defs, ["shapes/geometry.py:26 method Shape", "shapes/geometry.py:44 method Square", "shapes/geometry.py:59 method Circle"]);
    assert!(refs.contains(&"shapes/geometry.py:74 geometry::area_of".to_string()), "{refs:?}");
    assert_eq!(impls.len(), 2, "{impls:?}");
    let (defs, _, _) = answer(&mut s, "radius");
    assert_eq!(defs, ["shapes/geometry.py:57 field Circle"]);
    let (defs, _, _) = answer(&mut s, "squared");
    assert_eq!(defs, ["shapes/geometry.py:45 method Square"]);
    // End lines from the indentation blocks; the method is the caller.
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let items = code::file_items(&conn, &f.root.join("shapes/geometry.py").to_string_lossy()).unwrap().unwrap();
    let labels: Vec<String> = items.iter().map(|i| i.label()).collect();
    assert!(labels.contains(&"class geometry::Shape 15-34".to_string()) && labels.contains(&"fn Circle::area 59-64".to_string()), "{labels:?}");
    assert_eq!(code::enclosing(&items, 46).map(|i| i.name.as_str()), Some("Square::squared"));
}

#[test]
fn java_kinds_ranges_impl_edges_and_anonymous_members_from_the_scanner() {
    let f = fixture("java", &[]);
    let (rep, mut s) = indexed(&f, "shapes");
    assert_eq!(rep.rust, "java: prebuilt scip-java HEAD-20260402-1750");
    let (defs, refs, impls) = answer(&mut s, "Square");
    assert_eq!(defs, ["src/main/java/shapes/Square.java:6 class shapes", "src/main/java/shapes/Square.java:19 ctor Square"]);
    assert!(refs.contains(&"src/main/java/shapes/Main.java:15 Main::main".to_string()), "{refs:?}");
    assert_eq!(impls, ["6:Comparable for Square", "6:Shape for Square"]);
    // An anonymous class's method: SCIP keeps it local, the scanner fills
    // it in, under the definition around it.
    let (defs, _, impls) = answer(&mut s, "area");
    assert_eq!(
        defs,
        [
            "src/main/java/shapes/Shape.java:5 method Shape",
            "src/main/java/shapes/Main.java:11 method (scan) main",
            "src/main/java/shapes/Square.java:24 method Square"
        ]
    );
    assert_eq!(impls, ["5:Square::area for Shape::area", "24:Shape::area for Square::area"]);
    let (defs, _, _) = answer(&mut s, "compare");
    assert_eq!(defs, ["src/main/java/shapes/Square.java:11 method (scan) BY_SIDE"]);
    let (defs, _, _) = answer(&mut s, "edges");
    assert_eq!(defs, ["src/main/java/shapes/Square.java:17 field Square"]);
    // End lines from the brace matcher, past a `throws` line and an
    // `implements` line; the method is the caller.
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let items = code::file_items(&conn, &f.root.join("src/main/java/shapes/Square.java").to_string_lossy()).unwrap().unwrap();
    let labels: Vec<String> = items.iter().map(|i| i.label()).collect();
    for want in ["class shapes::Square 6-46", "field Square::BY_SIDE 8-14", "enum Square::Kind 33-40", "fn Square::unit 42-45"] {
        assert!(labels.contains(&want.to_string()), "{want}: {labels:?}");
    }
    assert_eq!(code::enclosing(&items, 44).map(|i| i.name.as_str()), Some("Square::unit"));
}
