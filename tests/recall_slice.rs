//! `recall` with an `h:` key: one slice of a stored output, as
//! `Handles::slice` (the `atlas slice` CLI) gives it, so a model never needs
//! the CLI; and every digest hint names that recall form.

mod common;

use std::path::Path;

use atlas::digest;
use atlas::handles::{HandleCfg, Handles, HintForm, Meta, Sel};
use atlas::json;
use atlas::json::{Map, Value};
use atlas::tools::Server;
use common::Fx;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/generic/run").join(name)).unwrap()
}

fn recall(s: &mut Server, args: Value) -> atlas::error::Result<String> {
    s.answer("recall", &args).map(atlas::tools::Answer::into_text)
}

fn ok(s: &mut Server, args: Value) -> String {
    recall(s, args.clone()).unwrap_or_else(|e| panic!("recall {args} failed: {e}"))
}

fn err(s: &mut Server, args: Value) -> String {
    match recall(s, args.clone()) {
        Ok(t) => panic!("recall {args} unexpectedly answered: {t}"),
        Err(e) => e.to_string(),
    }
}

/// A server over the fixture vaults (no memory store), and the handle
/// store beside its database, where `recall` looks without one.
fn setup(tag: &str) -> (Fx, Server, Handles) {
    let fx = Fx::new(tag);
    let s = fx.server();
    let h = Handles::open(&fx.db, HandleCfg::default()).unwrap();
    (fx, s, h)
}

fn cargo_test(h: &mut Handles) -> String {
    let m = Meta { kind: "run".into(), label: "cargo test".into(), cmd: "cargo test".into(), exit_code: Some(101), ..Meta::default() };
    h.put(fixture("cargo-test-fail.txt").as_bytes(), &m).unwrap().id
}

#[test]
fn recall_of_a_handle_with_each_selector_is_its_slice() {
    let (_fx, mut s, mut h) = setup("recall-slice-sel");
    let id = cargo_test(&mut h);
    let cases: Vec<(Value, Sel)> = vec![
        (json!({}), Sel::Whole),
        (json!({"lines": "17-18"}), Sel::Lines(17, 18)),
        (json!({"lines": "7"}), Sel::Lines(7, 7)),
        (json!({"item": "adds_wrong"}), Sel::Item("adds_wrong".into())),
        (json!({"item": "nope"}), Sel::Item("nope".into())),
        (json!({"failures": true}), Sel::Failures),
        (json!({"grep": "FAILED"}), Sel::Grep { re: "FAILED".into(), context: 2 }),
        (json!({"grep": "FAILED", "context": 1}), Sel::Grep { re: "FAILED".into(), context: 1 }),
        (json!({"head": 5}), Sel::Head(5)),
        (json!({"tail": 4}), Sel::Tail(4)),
        (json!({"page": 10}), Sel::Page(10)),
    ];
    for (extra, sel) in cases {
        let mut args = extra.as_object().cloned().unwrap_or_default();
        args.insert("key".into(), Value::String(id.clone()));
        let want = h.slice(&id, &sel).unwrap();
        assert_eq!(ok(&mut s, Value::Object(args.clone())), want, "{args:?}");
    }
    assert_eq!(ok(&mut s, json!({"key": id, "digest": true})), h.digest(&id).unwrap());
    // JSON asked for: the same text under `text`.
    let j = s.call("recall", &json!({"key": id, "lines": "17-18", "format": "json"})).unwrap();
    assert_eq!(j["text"].as_str(), Some(h.slice(&id, &Sel::Lines(17, 18)).unwrap().as_str()), "{j}");
}

/// The selectors may also follow the id inside `key`, as a hint line writes
/// them (`h:ID lines=A-B`), or as the CLI's flags (`h:ID --lines A-B`).
#[test]
fn selectors_written_into_the_key_are_taken() {
    let (_fx, mut s, mut h) = setup("recall-slice-key");
    let id = cargo_test(&mut h);
    let lines = h.slice(&id, &Sel::Lines(17, 18)).unwrap();
    assert_eq!(ok(&mut s, json!({"key": format!("{id} lines=17-18")})), lines);
    assert_eq!(ok(&mut s, json!({"key": format!("{id} --lines 17-18")})), lines);
    assert_eq!(ok(&mut s, json!({"key": format!("{id} failures")})), h.slice(&id, &Sel::Failures).unwrap());
    assert_eq!(ok(&mut s, json!({"key": format!("{id} failures=true")})), h.slice(&id, &Sel::Failures).unwrap());
    let g = Sel::Grep { re: "test result".into(), context: 2 };
    assert_eq!(ok(&mut s, json!({"key": format!("{id} grep=\"test result\"")})), h.slice(&id, &g).unwrap());
    // A number given as text is taken (a hint copied as it reads).
    assert_eq!(ok(&mut s, json!({"key": id, "head": "5"})), h.slice(&id, &Sel::Head(5)).unwrap());
}

#[test]
fn selector_arguments_on_any_other_key_are_refused() {
    let (_fx, mut s, mut h) = setup("recall-slice-refuse");
    let id = cargo_test(&mut h);
    for (k, v) in [
        ("lines", json!("1-5")),
        ("item", json!("x")),
        ("failures", json!(true)),
        ("grep", json!("x")),
        ("context", json!(2)),
        ("head", json!(3)),
        ("tail", json!(3)),
        ("page", json!(2)),
        ("digest", json!(true)),
    ] {
        let mut m = Map::new();
        m.insert("key".into(), json!("some-memory"));
        m.insert(k.into(), v.clone());
        let e = err(&mut s, Value::Object(m));
        assert!(e.contains(&format!("{k} ")) && e.contains("h:ID"), "{k}: {e}");
        let mut q = Map::new();
        q.insert("query".into(), json!("widget"));
        q.insert(k.into(), v);
        let e = err(&mut s, Value::Object(q));
        assert!(e.contains(k) && e.contains("h:ID"), "{k}: {e}");
    }
    // Two selectors at once, or one that does not parse, are refused too.
    let e = err(&mut s, json!({"key": id, "lines": "1-3", "item": "adds"}));
    assert!(e.contains("lines") && e.contains("item"), "{e}");
    let e = err(&mut s, json!({"key": id, "lines": "9-2"}));
    assert!(e.contains("runs backwards"), "{e}");
    let e = err(&mut s, json!({"key": id, "query": "x"}));
    assert!(e.contains("query"), "{e}");
    let e = err(&mut s, json!({"key": format!("{id} bogus=1")}));
    assert!(e.contains("bogus"), "{e}");
}

#[test]
fn an_unknown_handle_names_the_listing_and_the_listing_names_the_newest() {
    let (_fx, mut s, mut h) = setup("recall-slice-unknown");
    let e = err(&mut s, json!({"key": "h:0000000000"}));
    assert!(e.contains("h:0000000000") && e.contains("recall key=h:"), "{e}");
    assert_eq!(ok(&mut s, json!({"key": "h:"})), "no stored outputs\n");
    let id = cargo_test(&mut h);
    let list = ok(&mut s, json!({"key": "h:"}));
    assert!(list.starts_with(&format!("{id} run ")) && list.contains("cargo test"), "{list}");
    let other = h.put(b"one\ntwo\n", &Meta { kind: "run".into(), label: "echo".into(), ..Meta::default() }).unwrap().id;
    let list = ok(&mut s, json!({"key": "h:", "limit": 1}));
    assert_eq!(list.lines().count(), 1, "{list}");
    assert!(list.starts_with(&other), "{list}");
}

/// With the memory store configured, handles are where the hooks keep
/// them: beside the memory settings' index database.
#[test]
fn with_memory_the_handles_beside_its_index_are_read() {
    let fx = Fx::new("recall-slice-mem");
    let mut s = fx.server();
    let db = fx.tmp.path().join("hookstate").join("index.sqlite");
    s.set_memory(atlas::memory::settings::Settings::from_config(None, &db).unwrap());
    let mut h = Handles::open(&db, HandleCfg::default()).unwrap();
    let id = cargo_test(&mut h);
    assert_eq!(ok(&mut s, json!({"key": id, "lines": "17-18"})), h.slice(&id, &Sel::Lines(17, 18)).unwrap());
}

/// A hint's placeholder filled in: `NAME` and `PATH` with `item`, `A-B`
/// with `1-3`, `RE` with `.`, `error:N` with `error:1`.
fn fill<'a>(v: &'a str, item: &'a str) -> &'a str {
    match v {
        "NAME" | "PATH" => item,
        "A-B" => "1-3",
        "RE" => ".",
        "error:N" => "error:1",
        "warning:N" => "warning:1",
        v => v,
    }
}

/// The selection a hint's `name=value` means, read here on its own (not by
/// the parser under test): a value's surrounding quotes are the hint's.
fn meant(k: &str, v: &str) -> Sel {
    let v = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v);
    let num = |v: &str| v.parse::<usize>().unwrap_or_else(|_| panic!("{k}={v}"));
    match k {
        "lines" => {
            let (a, b) = v.split_once('-').unwrap_or((v, v));
            Sel::Lines(num(a), num(b))
        }
        "item" => Sel::Item(v.to_string()),
        "grep" => Sel::Grep { re: v.to_string(), context: 2 },
        "failures" => Sel::Failures,
        "head" => Sel::Head(num(v)),
        "tail" => Sel::Tail(num(v)),
        "page" => Sel::Page(num(v)),
        o => panic!("no selector {o}"),
    }
}

/// Each `recall h:ID ...` a line names: its arguments as a model copies
/// them (a value as written, quotes and all), the handle, and the
/// selection it means. One per alternative (`a=b | c=d`), placeholders
/// filled in.
fn hint_calls(line: &str, item: &str) -> Vec<(Value, String, Sel)> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(i) = rest.find("recall h:") {
        let s = &rest[i + "recall ".len()..];
        let end = s.find([')', ';']).unwrap_or(s.len());
        let call = s[..end].trim();
        rest = &s[end..];
        let (key, sels) = call.split_once(' ').unwrap_or((call, ""));
        if sels.is_empty() {
            out.push((json!({"key": key}), key.to_string(), Sel::Whole));
            continue;
        }
        for alt in sels.split(" | ") {
            let (k, v) = alt.split_once('=').unwrap_or_else(|| panic!("{alt:?} in {line:?} is not name=value"));
            let v = fill(v, item);
            let val = if v == "true" {
                Value::Bool(true)
            } else if let Ok(n) = v.parse::<u64>() {
                json!(n)
            } else {
                Value::String(v.to_string())
            };
            let mut m = Map::new();
            m.insert("key".into(), Value::String(key.to_string()));
            m.insert(k.to_string(), val);
            out.push((Value::Object(m), key.to_string(), meant(k, v)));
        }
    }
    out
}

/// Each `atlas slice h:ID ...` a line names (the CLI form of the hints),
/// as the handle and the selection `atlas slice` parses from the words a
/// shell gives it (quotes removed). One per alternative.
fn cli_calls(line: &str, item: &str) -> Vec<(String, Sel)> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(i) = rest.find("atlas slice h:") {
        let s = &rest[i + "atlas slice ".len()..];
        let end = s.find([')', ';']).unwrap_or(s.len());
        let call = s[..end].trim();
        rest = &s[end..];
        let (key, sels) = call.split_once(' ').unwrap_or((call, ""));
        if sels.is_empty() {
            out.push((key.to_string(), atlas::handles::parse_sel(&[]).unwrap()));
            continue;
        }
        for alt in sels.split(" | ") {
            let words: Vec<String> = match alt.split_once(' ') {
                Some((flag, v)) => vec![flag.to_string(), fill(v.trim_matches('"'), item).to_string()],
                None => vec![alt.to_string()],
            };
            out.push((key.to_string(), atlas::handles::parse_sel(&words).unwrap_or_else(|e| panic!("{alt:?} in {line:?}: {e}"))));
        }
    }
    out
}

/// A recall answer that selected something: not a "nothing selected" or
/// "no item" reply.
fn selected(t: &str) -> bool {
    !t.is_empty() && !t.contains(": nothing selected") && !t.contains(": no item ") && !t.contains(": no failure blocks")
}

/// Every hint a digest or a capped slice writes names `recall`, never the
/// CLI, and each one, copied into recall's arguments, answers with the
/// selection it names; its CLI form (on a terminal) parses to that same
/// selection.
#[test]
fn every_hint_names_recall_and_round_trips() {
    let (_fx, mut s, mut h) = setup("recall-slice-hints");
    let mut src = String::from("//! A module.\n\npub struct Widget {\n    count: u32,\n}\n\nimpl Widget {\n");
    for i in 0..40 {
        src.push_str(&format!("    pub fn step_{i}(&mut self) -> u32 {{\n        self.count += {i};\n        self.count\n    }}\n\n"));
    }
    src.push_str("}\n");
    let generic: String = (0..3000).map(|i| format!("line {i} of a long output\n")).collect();
    let stored = [
        (
            fixture("cargo-test-fail.txt"),
            Meta { kind: "run".into(), label: "cargo test".into(), cmd: "cargo test".into(), exit_code: Some(101), ..Meta::default() },
        ),
        (
            fixture("cargo-build-fail.txt"),
            Meta { kind: "run".into(), label: "cargo build".into(), cmd: "cargo build".into(), exit_code: Some(101), ..Meta::default() },
        ),
        (src, Meta { kind: "read".into(), label: "src/widget.rs".into(), cmd: "src/widget.rs".into(), ..Meta::default() }),
        (generic, Meta { kind: "run".into(), label: "cat big".into(), cmd: "cat big".into(), exit_code: Some(0), ..Meta::default() }),
    ];
    let mut calls = 0;
    for (text, m) in &stored {
        let row = h.put(text.as_bytes(), m).unwrap();
        let shape = Handles::shape_of(&row, text);
        let item = digest::items(shape, text).first().map(|i| i.name.clone()).unwrap_or_else(|| "x".into());
        let d = h.digest(&row.id).unwrap();
        assert!(!d.contains("atlas slice"), "{d}");
        let last = d.trim_end().lines().last().unwrap_or("");
        assert!(last.starts_with(&format!("recall {} ", row.id)), "the last line is the hint: {d}");
        let cli = HintForm::Cli.apply(&d);
        assert!(!cli.contains("recall h:"), "{cli}");
        assert_eq!(HintForm::Recall.apply(&d), d);
        assert_eq!(cli.lines().count(), d.lines().count());
        for (line, cline) in d.lines().zip(cli.lines()) {
            let want = hint_calls(line, &item);
            for (args, key, sel) in &want {
                let t = ok(&mut s, args.clone());
                assert_eq!(t, h.slice(key, sel).unwrap(), "{args} means {sel:?}");
                if !matches!(sel, Sel::Item(n) if n == "x") {
                    assert!(selected(&t), "{args}: {t}");
                }
                calls += 1;
            }
            let got: Vec<(String, Sel)> = cli_calls(cline, &item);
            assert_eq!(got, want.iter().map(|(_, k, s)| (k.clone(), s.clone())).collect::<Vec<_>>(), "{line:?} vs {cline:?}");
        }
    }
    assert!(calls >= 12, "{calls} hint calls");
    // A capped slice names the rest as a recall of its lines.
    let mut small = Handles::open(&_fx.db, HandleCfg { slice_bytes: 256, ..HandleCfg::default() }).unwrap();
    let id = h.put(stored[3].0.as_bytes(), &stored[3].1).unwrap().id;
    let cut = small.slice(&id, &Sel::Lines(1, 3000)).unwrap();
    let note = cut.lines().last().unwrap();
    assert!(note.starts_with("truncated, ") && !note.contains("atlas slice"), "{note}");
    let next = hint_calls(note, "");
    assert_eq!(next.len(), 1, "{note}");
    assert_eq!(ok(&mut s, next[0].0.clone()), h.slice(&id, &next[0].2).unwrap());
    assert!(matches!(next[0].2, Sel::Lines(a, 3000) if a > 1), "{note}");
    assert_eq!(cli_calls(&HintForm::Cli.apply(note), ""), vec![(next[0].1.clone(), next[0].2.clone())]);
    let _ = small.evict();
    // A test run with build errors names the first one.
    let both = format!("{}{}", fixture("cargo-build-fail.txt"), fixture("cargo-test-fail.txt"));
    let row = h.put(both.as_bytes(), &stored[0].1).unwrap();
    let d = h.digest(&row.id).unwrap();
    let build = d.lines().find(|l| l.starts_with("build: ")).unwrap_or_else(|| panic!("{d}"));
    let c = hint_calls(build, "");
    assert_eq!(c.len(), 1, "{build}");
    assert_eq!(c[0].0, json!({"key": row.id, "item": "error:1"}), "{build}");
    let t = ok(&mut s, c[0].0.clone());
    assert!(selected(&t), "{t}");
    assert_eq!(t, h.slice(&row.id, &Sel::Item("error:1".into())).unwrap());
}

/// A diff of `n` one-hunk files, the last with a second hunk of one added
/// line of `long` bytes.
fn pr_diff(n: usize, long: usize) -> String {
    let mut d = String::new();
    for i in 0..n {
        d.push_str(&format!("diff --git a/f{i}.txt b/f{i}.txt\nindex 1111111..2222222 100644\n--- a/f{i}.txt\n+++ b/f{i}.txt\n"));
        d.push_str(&format!("@@ -1,3 +1,3 @@\n keep {i}\n-old {i}\n+new {i}\n tail {i}\n"));
        if i + 1 == n && long > 0 {
            d.push_str(&format!("@@ -10,1 +10,2 @@\n ctx\n+{}\n", "x".repeat(long)));
        }
    }
    d
}

/// The PR digest's hints (`more files ... grep=`, `a file: recall h:F`,
/// `a hunk: recall h:F lines=A-B`) round-trip through the server as the
/// selections they name, and their CLI forms parse to the same.
#[test]
fn pr_digest_hints_round_trip() {
    let (_fx, mut s, mut h) = setup("recall-slice-pr");
    let w = atlas::pr_digest::Where { key: "octo/demo#1".into(), attributes: String::new(), toplevel: String::new() };
    let meta = Meta { kind: "bash".into(), label: "gh pr diff 1".into(), cmd: "gh pr diff 1".into(), ..Meta::default() };
    let text = pr_diff(atlas::pr_digest::MAX_ROWS + 5, 0);
    let d = atlas::pr_digest::diff(&mut h, &w, &text, &meta).unwrap().digest;
    // More files than rows: a grep of the whole output for the file headers.
    let more = d.lines().find(|l| l.contains(" more files: recall ")).unwrap_or_else(|| panic!("{d}"));
    let c = hint_calls(more, "");
    assert_eq!(c.len(), 1, "{more}");
    assert_eq!(c[0].2, Sel::Grep { re: "^diff --git".into(), context: 2 }, "{more}");
    let t = ok(&mut s, c[0].0.clone());
    assert!(selected(&t) && t.contains("diff --git a/f0.txt"), "{t}");
    assert_eq!(t, h.slice(&c[0].1, &c[0].2).unwrap());
    assert_eq!(cli_calls(&HintForm::Cli.apply(more), ""), vec![(c[0].1.clone(), c[0].2.clone())]);
    // A file and a hunk: the footer's h:F and A-B, from the first row.
    let row = d.lines().find(|l| l.starts_with("M f0.txt ")).unwrap_or_else(|| panic!("{d}"));
    let file = row.split(' ').find(|w| w.starts_with("h:")).unwrap();
    let hunk = row.split_once(" @ ").map(|(_, r)| r.split(' ').next().unwrap()).unwrap_or_else(|| panic!("{row}"));
    let footer = d.lines().find(|l| l.starts_with("a file: recall h:F")).unwrap_or_else(|| panic!("{d}"));
    let filled = footer.replace("h:F", file).replace("lines=A-B", &format!("lines={hunk}"));
    let c = hint_calls(&filled, "");
    assert_eq!(c.iter().map(|x| x.2.clone()).collect::<Vec<_>>(), vec![Sel::Whole, meant("lines", hunk)], "{filled}");
    for (args, key, sel) in &c {
        let t = ok(&mut s, args.clone());
        assert!(selected(&t) && t.contains("new") && t.contains("keep 0"), "{args}: {t}");
        assert_eq!(t, h.slice(key, sel).unwrap(), "{args}");
    }
    assert_eq!(cli_calls(&HintForm::Cli.apply(&filled), ""), c.iter().map(|(_, k, s)| (k.clone(), s.clone())).collect::<Vec<_>>());
}

/// A line alone over the slice cap is cut to it, with a note naming its
/// size and a grep that shows the part around a match: in any output, and
/// in a PR diff's file part, by default and by its lines.
#[test]
fn a_line_over_the_cap_is_cut_with_a_note() {
    let (_fx, mut s, mut h) = setup("recall-slice-long");
    let cap = HandleCfg::default().slice_bytes;
    let mut one = "a".repeat(200_000);
    one.push_str("NEEDLE");
    one.push_str(&"b".repeat(100_000));
    let text = format!("{one}\nshort line\n");
    let id = h
        .put(text.as_bytes(), &Meta { kind: "run".into(), label: "json".into(), cmd: "cat one.json".into(), ..Meta::default() })
        .unwrap()
        .id;
    for args in [json!({"key": id}), json!({"key": id, "lines": "1"}), json!({"key": id, "head": 2})] {
        let t = ok(&mut s, args.clone());
        assert!(t.len() < cap + 400, "{args}: {} bytes", t.len());
        assert!(
            t.contains("line 1 is 300006 bytes") && t.contains(&format!("recall {id} grep=")),
            "{args}: {}",
            &t[t.len().saturating_sub(300)..]
        );
        // The line after it is named for the next call.
        assert!(
            t.trim_end().ends_with(&format!("(recall {id} lines=2-2)")) || args.get("lines").is_some(),
            "{args}: {}",
            &t[t.len().saturating_sub(300)..]
        );
    }
    let g = ok(&mut s, json!({"key": id, "grep": "NEEDLE", "context": 0}));
    assert!(g.len() < cap + 400 && g.contains("NEEDLE") && g.contains("around the first match"), "{} bytes", g.len());
    // A PR file part with one long added line.
    let w = atlas::pr_digest::Where { key: "octo/demo#2".into(), attributes: String::new(), toplevel: String::new() };
    let meta = Meta { kind: "bash".into(), label: "gh pr diff 2".into(), cmd: "gh pr diff 2".into(), ..Meta::default() };
    let d = atlas::pr_digest::diff(&mut h, &w, &pr_diff(1, 50_000), &meta).unwrap().digest;
    let row = d.lines().find(|l| l.starts_with("M f0.txt ")).unwrap_or_else(|| panic!("{d}"));
    let file = row.split(' ').find(|w| w.starts_with("h:")).unwrap().to_string();
    let n = h.text(&file).unwrap().1.lines().count();
    for args in [json!({"key": file}), json!({"key": file, "lines": format!("1-{n}")})] {
        // The first page ends before the long line's hunk; its next page
        // holds that line, cut.
        let first = ok(&mut s, args.clone());
        let next = hint_calls(first.lines().last().unwrap_or(""), "");
        assert_eq!(next.len(), 1, "{args}: {first}");
        let t = ok(&mut s, next[0].0.clone());
        assert!(t.len() < cap + 400, "{args}: {} bytes", t.len());
        assert!(
            t.contains(" is 50001 bytes") && t.contains(&format!("recall {file} grep=")),
            "{args}: {}",
            &t[t.len().saturating_sub(300)..]
        );
    }
}

/// A value-taking selector with no value is refused, not read as "true";
/// a pasted hint line with `|` asks for one selector; a key written as a
/// hint or a command (`recall h:ID ...`, `atlas slice h:ID ...`) is taken;
/// a separate value in quotes loses them.
#[test]
fn malformed_and_pasted_keys_are_refused_or_taken() {
    let (_fx, mut s, mut h) = setup("recall-slice-pasted");
    let id = cargo_test(&mut h);
    for k in ["grep", "lines", "item", "head", "tail", "page"] {
        let e = err(&mut s, json!({"key": format!("{id} {k}")}));
        assert!(e.contains(&format!("{k} needs a value: {k}=")), "{k}: {e}");
        let mut m = Map::new();
        m.insert("key".into(), json!(id));
        m.insert(k.into(), Value::Bool(true));
        let e = err(&mut s, Value::Object(m));
        assert!(e.contains(&format!("{k} takes a value")), "{k}: {e}");
    }
    let e = err(&mut s, json!({"key": format!("{id} grep=x context")}));
    assert!(e.contains("context needs a value"), "{e}");
    let e = err(&mut s, json!({"key": format!("{id} failures=true | item=NAME | lines=A-B")}));
    assert!(e.contains("one selector at a time"), "{e}");
    let lines = h.slice(&id, &Sel::Lines(17, 18)).unwrap();
    for k in [
        format!("recall {id} lines=17-18"),
        format!("recall key={id} lines=17-18"),
        format!("atlas slice {id} --lines 17-18"),
        format!("  {id}   lines=17-18 "),
    ] {
        assert_eq!(ok(&mut s, json!({"key": k})), lines, "{k}");
    }
    let g = Sel::Grep { re: "test result".into(), context: 2 };
    assert_eq!(ok(&mut s, json!({"key": id, "grep": "\"test result\""})), h.slice(&id, &g).unwrap());
    assert_eq!(ok(&mut s, json!({"key": format!("{id} grep=\"test result\"")})), h.slice(&id, &g).unwrap());
}

/// Under projects the hooks store an output by the event's folder, which
/// can be another project than the server's: recall finds it there, and
/// names the case when no project has it.
#[test]
fn under_projects_another_projects_handles_are_found() {
    let fx = Fx::new("recall-slice-projects");
    let mut s = fx.server();
    let projects = fx.tmp.path().join("projects");
    let mine = projects.join("mine").join("atlas.sqlite");
    let mut settings = atlas::memory::settings::Settings::from_config(None, &mine).unwrap();
    settings.user_state = Some(projects.clone());
    s.set_memory(settings);
    let _ = Handles::open(&mine, HandleCfg::default()).unwrap();
    let mut other = Handles::open(&projects.join("other").join("atlas.sqlite"), HandleCfg::default()).unwrap();
    let id = cargo_test(&mut other);
    assert_eq!(ok(&mut s, json!({"key": id, "lines": "17-18"})), other.slice(&id, &Sel::Lines(17, 18)).unwrap());
    assert_eq!(ok(&mut s, json!({"key": id, "digest": true})), other.digest(&id).unwrap());
    let e = err(&mut s, json!({"key": "h:0000000000"}));
    assert!(e.contains("under projects") && e.contains("mine") && e.contains("recall key=h:"), "{e}");
}

/// The listed description names the handle form (its selectors stay out of
/// the listed schema, which is at its budget; every hint line names them),
/// and the full schema and the help name every selector.
#[test]
fn the_recall_schema_and_help_name_the_handle_form() {
    let listed = atlas::tools::tool_list();
    let r = listed.iter().find(|t| t["name"] == "recall").unwrap();
    assert!(r["description"].as_str().unwrap().contains("h:ID"), "{r}");
    let full = atlas::tools::full_tool_list().into_iter().find(|t| t["name"] == "recall").unwrap();
    for k in atlas::handles::SLICE_ARGS {
        assert!(full["inputSchema"]["properties"].get(*k).is_some(), "{k} in {full}");
    }
    let help = atlas::tools::help();
    assert!(help.contains("`h:ID`") && help.contains("`h:`"), "{help}");
    let grep = atlas::tools::full_tool_list().into_iter().find(|t| t["name"] == "grep").unwrap();
    assert!(!grep["description"].as_str().unwrap().contains("atlas slice"), "{grep}");
}
