//! Tests of `explain.rs`: the known strings of a file and the verdicts.

use super::*;

fn strings(name: &str, text: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    scan(name, text, &mut |b, l, _| out.push((String::from_utf8_lossy(&b).into_owned(), l)));
    out
}

/// The literal values of a file alone.
fn literals(name: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    scan(name, text, &mut |b, _, lit| {
        if lit {
            out.push(String::from_utf8_lossy(&b).into_owned());
        }
    });
    out
}

fn has(v: &[(String, usize)], s: &str) -> bool {
    v.iter().any(|(x, _)| x == s)
}

#[test]
fn rust_literals_are_decoded_and_format_strings_split() {
    let src = "/// Help text for the flag.\nfn main() {\n    let a = \"tab\\there \\\"q\\\" \\u{e9}\\x41\";\n    let r = r#\"raw \"x\" ok\"#;\n    let b = b\"bytes here\";\n    println!(\"left {} right {{lit}} end\", 1);\n    let c = 'x'; let d = '\\''; let e = '\"';\n    fn g<'a>(s: &'a str) {}\n    let m = \"two\n lines\";\n    let k = \"cont\\\n        inued\";\n}\n";
    let v = strings("src/main.rs", src);
    for s in ["Help text for the flag.", "main", "tab\there \"q\" \u{e9}A", "raw \"x\" ok", "bytes here", "left ", " right {lit} end", "left {} right {{lit}} end", "two\n lines", "continued"] {
        assert!(has(&v, s), "{s:?} in {v:?}");
    }
    // The char literal with a quote did not open a string.
    assert!(!v.iter().any(|(s, _)| s.contains(";\n")), "{v:?}");
    // Lines are counted: the raw string is on line 4, the joined one on 11.
    assert!(v.contains(&("raw \"x\" ok".to_string(), 4)) && v.contains(&("continued".to_string(), 11)), "{v:?}");
    // Raw identifiers are identifiers.
    assert!(has(&strings("a.rs", "let r#type = 1;"), "type"));
}

#[test]
fn c_literals_and_other_text_files() {
    let v = strings("sqlite3.c", "/* A comment\n** line two */\nstatic const char *z = \"oct\\101l \\x41 q\";\nchar c = '\"';\n");
    for s in ["A comment", "line two", "octAl A q", "static", "char"] {
        assert!(has(&v, s), "{s:?} in {v:?}");
    }
    // Any other text file: whole (include_str!) and by line.
    let v = strings("templates/a.md", "# Title\n\n  body line  \n");
    assert!(has(&v, "# Title\n\n  body line  \n") && has(&v, "body line") && has(&v, "# Title"), "{v:?}");
}

/// A corpus of one file per part, `src/lib.rs` of its crate.
fn corpus(parts: &[(Kind, &str, &str)]) -> Corpus {
    let mut c = Corpus::default();
    for (k, name, text) in parts {
        c.add_text(Origin { kind: *k, krate: name.to_string(), name: name.to_string(), file: "src/lib.rs".into() }, text);
    }
    c
}

#[test]
fn verdicts() {
    let c = corpus(&[(Kind::Tree, "tree", "const A: &str = \"lorem quill\";"), (Kind::Dep, "quire", "const B: &str = \"feather ipsum\";")]);
    let bin = b"\0lorem quillfeather ipsum\0/home/quillfeather\0";
    let Verdict::Split(ps) = c.explain(bin, 7, 19) else { panic!("{:?}", c.explain(bin, 7, 19)) };
    assert_eq!(ps.iter().map(|p| (p.string.as_slice(), p.origin.kind, p.literal)).collect::<Vec<_>>(), vec![(&b"lorem quill"[..], Kind::Tree, true), (&b"feather ipsum"[..], Kind::Dep, true)]);
    assert_eq!(c.explain(bin, 32, 44), Verdict::Unexplained);
    assert!(matches!(c.explain(bin, 1, 7), Verdict::Contained { .. }));
    // The strictest source wins a string two of them hold.
    let c = corpus(&[(Kind::Toolchain, "rust", "\"x quillfeather y\""), (Kind::Dep, "quire", "\"x quillfeather y\""), (Kind::Tree, "tree", "\"x quillfeather y\"")]);
    let Verdict::Contained { origin, .. } = c.explain(b"x quillfeather y", 2, 14) else { panic!() };
    assert_eq!(origin.kind, Kind::Tree);
    // And of two different strings that each hold the place.
    for order in [[(Kind::Dep, "quire", "\"quillfeather\""), (Kind::Tree, "tree", "\"x quillfeather y\"")], [(Kind::Dep, "quire", "\"x quillfeather\""), (Kind::Tree, "tree", "\"quillfeather y\"")]] {
        let Verdict::Contained { origin, .. } = corpus(&order).explain(b"x quillfeather y", 2, 14) else { panic!() };
        assert_eq!(origin.kind, Kind::Tree, "{order:?}");
    }
    // An empty corpus explains nothing; out of range is unexplained.
    assert_eq!(Corpus::default().explain(b"abcdef", 1, 3), Verdict::Unexplained);
    assert_eq!(c.explain(b"abc", 2, 9), Verdict::Unexplained);
}

#[test]
fn metadata_packages_outside_the_tree_are_dependencies() {
    let d = std::env::temp_dir().join(format!("xtask-explain-{}", std::process::id()));
    let tree = d.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::create_dir_all(d.join("reg/quire-1.0.0")).unwrap();
    let j = |p: &Path| atlas::json::to_string(&p.join("Cargo.toml").display().to_string());
    let json = format!(
        "{{\"packages\":[{{\"name\":\"atlas\",\"version\":\"1.0.0\",\"manifest_path\":{}}},{{\"name\":\"sub\",\"version\":\"0.1.0\",\"manifest_path\":{}}},{{\"name\":\"quire\",\"version\":\"1.0.0\",\"manifest_path\":{}}}]}}",
        j(&tree),
        j(&tree.join("sub")),
        j(&d.join("reg/quire-1.0.0"))
    );
    let deps = packages(&json, &tree).unwrap();
    // A path package outside the tree is the owner's code: as strict as it.
    assert_eq!(deps.iter().map(|(n, v, _, k)| (format!("{n} {v}"), *k)).collect::<Vec<_>>(), vec![("quire 1.0.0".to_string(), Kind::Tree)]);
    let json = json.replace("\"name\":\"quire\"", "\"source\":\"registry+https://github.com/rust-lang/crates.io-index\",\"name\":\"quire\"");
    assert_eq!(packages(&json, &tree).unwrap()[0].3, Kind::Dep);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn joined_literals_are_one_value() {
    // concat! joins its literal arguments; a macro call or a nested bracket
    // ends a run; numbers and bools are written as their text.
    let v = literals("a.rs", "const A: &str = concat!(\"lorem quill\", // c\n \"feather\", 1, true);\nconst B: &str = concat! [env!(\"X\"), \"one\", \"two\"];\nconst C: &str = concat!(\"ab\" \"cd\");");
    for s in ["lorem quillfeather1true", "onetwo"] {
        assert!(v.contains(&s.to_string()), "{s:?} in {v:?}");
    }
    assert!(!v.iter().any(|s| s.contains("Xone")), "{v:?}");
    // C joins adjacent literals, across comments and lines, not over a comma.
    let v = literals("a.c", "const char *z = \"lorem quill\" /* c */\n  \"feather\";\nf(\"one\", \"two\");\n");
    assert!(v.contains(&"lorem quillfeather".to_string()) && !v.contains(&"onetwo".to_string()), "{v:?}");
    // Identifiers, comments and a text file's lines are not literal values;
    // the whole text file is.
    assert_eq!(literals("a.rs", "// lorem\nfn quill() {}"), Vec::<String>::new());
    assert_eq!(literals("a.md", "one\ntwo\n"), vec!["one\ntwo\n".to_string()]);
}

#[test]
fn a_chain_is_whole_literals_end_to_end() {
    fn tree(t: &str) -> (Kind, &'static str, &str) {
        (Kind::Tree, "tree", t)
    }
    // Overlapping pieces never make a chain.
    let c = corpus(&[tree("\"Pin\""), (Kind::Dep, "a", "\"necr\""), (Kind::Dep, "b", "\"rest\"")]);
    assert_eq!(c.explain(b"/Pinecrest/", 1, 10), Verdict::Unexplained);
    // Whole pieces end to end do, from any byte before the place.
    let c = corpus(&[tree("\"/Pin\""), (Kind::Dep, "a", "\"ecr\""), (Kind::Dep, "b", "\"est/\"")]);
    assert!(matches!(c.explain(b"/Pinecrest/", 1, 10), Verdict::Split(ps) if ps.len() == 3));
    // A piece that is not a literal value: a join, which stops.
    let c = corpus(&[tree("fn Pine() {}"), (Kind::Dep, "a", "\"crest/\"")]);
    assert!(matches!(c.explain(b"/Pinecrest/", 1, 10), Verdict::Join(ps, _) if !ps[0].literal));
    // The toolchain's identifiers are not kept at all.
    let c = corpus(&[(Kind::Toolchain, "rust", "fn Pine() {}"), (Kind::Dep, "a", "\"crest/\"")]);
    assert_eq!(c.explain(b"/Pinecrest/", 1, 10), Verdict::Unexplained);
    // Two pieces of one file side by side: a join.
    let c = corpus(&[tree("const A: &str = \"Pine\"; const B: &str = \"crest\";")]);
    assert!(matches!(c.explain(b"/Pinecrest/", 1, 10), Verdict::Join(..)));
    // A literal that holds the place says how much it holds beyond it.
    let c = corpus(&[(Kind::Dep, "a", "\"Pinecrest\""), (Kind::Dep, "b", "\"by Pinecrest\"")]);
    assert!(matches!(c.explain(b"/Pinecrest/", 1, 10), Verdict::Contained { literal: true, context: 0, .. }));
    // Of two that hold it, the longer: the literal is at least that long.
    assert!(matches!(c.explain(b"by Pinecrest", 3, 12), Verdict::Contained { literal: true, context: 3, .. }));
}

#[test]
fn a_checkout_is_read_at_head() {
    let d = std::env::temp_dir().join(format!("xtask-explain-head-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let g = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&d).args(args).output().unwrap().status.success(), "{args:?}");
    g(&["init", "-q"]);
    std::fs::write(d.join("a.rs"), "const A: &str = \"x quillfeather y\";\n").unwrap();
    g(&["add", "a.rs"]);
    g(&["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "a"]);
    // The working file says something else: HEAD is what is built.
    std::fs::write(d.join("a.rs"), "const A: &str = \"other\";\n").unwrap();
    let c = Corpus::release(&d).unwrap();
    assert!(matches!(c.explain(b"x quillfeather y", 2, 14), Verdict::Contained { .. }), "{:?}", c.explain(b"x quillfeather y", 2, 14));
    let _ = std::fs::remove_dir_all(&d);
}

/// The rule this file replaced: every byte of the place in some occurrence
/// of any known string, overlaps allowed. For the measurement below only.
fn union_covered(c: &Corpus, bin: &[u8], start: usize, end: usize) -> bool {
    let lo = start.saturating_sub(c.max_len.min(MAX_BACK));
    let lo = bin[lo..start].iter().rposition(|&b| b == 0).map_or(lo, |k| lo + k + 1);
    let mut covered = vec![false; end - start];
    let mut ids = Vec::new();
    for p in lo..end {
        c.at(bin, p, &mut ids);
        for &id in &ids {
            let q = p + c.strings[id as usize].bytes.len();
            if q > start {
                covered[p.max(start) - start..q.min(end) - start].iter_mut().for_each(|x| *x = true);
            }
        }
    }
    covered.iter().all(|&x| x)
}

/// The false-negative measurement: synthetic names (`KIND NAME` lines of
/// the file XTASK_FN_NAMES) put into build paths, judged against the corpus
/// of the release tree XTASK_FN_SRC, with bytes of the binary XTASK_FN_BIN
/// around them. `cargo test -p xtask -- --ignored false_negatives`.
#[test]
#[ignore]
fn false_negatives() {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    let c = Corpus::release(Path::new(&var("XTASK_FN_SRC"))).unwrap();
    eprintln!("{}", c.summary.join("; "));
    let real = std::fs::read(var("XTASK_FN_BIN")).unwrap();
    let names = std::fs::read_to_string(var("XTASK_FN_NAMES")).unwrap();
    let mut seed = 382u64;
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let mut tally: std::collections::BTreeMap<(String, &str, &str), usize> = Default::default();
    for l in names.lines() {
        let Some((class, name)) = l.split_once(' ') else { continue };
        let at = next() % (real.len() - 512);
        let around = &real[at..at + 256];
        let shapes: [(&str, Vec<u8>, String); 3] = [
            ("unix", b"\0/home/".to_vec(), "/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.228/src/de/mod.rs\0".into()),
            ("windows", b"\0C:\\Users\\".to_vec(), "\\.cargo\\registry\\src\\index.crates.io-1949cf8c6b5b557f\\serde-1.0.228\\src\\lib.rs\0".into()),
            ("packed", [around, b"/home/"].concat(), format!("/src/main.rs{}", String::from_utf8_lossy(&real[at + 256..at + 512]))),
        ];
        for (shape, pre, post) in shapes {
            let bin = [pre.as_slice(), name.as_bytes(), post.as_bytes()].concat();
            let (s, e) = (pre.len(), pre.len() + name.len());
            let new = match c.explain(&bin, s, e) {
                Verdict::Split(ps) => {
                    eprintln!("SPLIT {class} {shape} {name}: {}", ps.iter().map(|p| String::from_utf8_lossy(&p.string).into_owned()).collect::<Vec<_>>().join(" + "));
                    "split (passes)"
                }
                Verdict::Join(..) => "join (stops, entry)",
                Verdict::Contained { origin, literal, context, .. } if origin.kind != Kind::Tree && literal && context >= MIN_CONTEXT => "contained, entry possible",
                Verdict::Contained { .. } => "contained (stops)",
                Verdict::Unexplained => "unexplained (stops)",
            };
            let old = if union_covered(&c, &bin, s, e) { "old: passes" } else { "old: stops" };
            *tally.entry((format!("{class} {shape}"), new, old)).or_default() += 1;
        }
    }
    for ((k, new, old), n) in &tally {
        eprintln!("{k:14} {new:28} {old:12} {n}");
    }
}
