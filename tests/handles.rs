//! The handle store, the digests over recorded cargo output, the PostToolUse
//! routing swap and `atlas run`.

mod common;

use std::path::Path;

use atlas::digest::{self, Shape};
use atlas::handles::{HandleCfg, Handles, Meta, Sel};
use atlas::hook_rewrite::{relative, rewrite, routable, set_routing, unwrite, Router, EXIT_MARK};
use atlas::hooks::HookEvent;
use atlas::json;
use atlas::json::Value;
use common::Temp;

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/generic/run").join(name);
    std::fs::read_to_string(p).unwrap()
}

fn store(t: &Temp) -> Handles {
    Handles::open(&t.path().join("index.sqlite"), HandleCfg::default()).unwrap()
}

#[test]
fn a_failing_cargo_test_digests_to_its_failures() {
    let text = fixture("cargo-test-fail.txt");
    assert_eq!(digest::shape("bash", "cargo test -j 8", &text), Shape::CargoTest);
    let d = digest::digest("h:0123456789", "cargo test", Shape::CargoTest, &text, Some(101), 600);
    assert!(d.starts_with("h:0123456789 cargo test exit 101,"), "{d}");
    assert!(d.contains("tests 1 passed 2 failed 1 ignored (1 binaries): failed"), "{d}");
    assert!(d.contains("FAILED tests::adds_wrong src/lib.rs:7 assertion `left == right` failed (left: 3, right: 4)"), "{d}");
    assert!(d.contains("FAILED tests::panics_here src/lib.rs:8 the widget count went negative"), "{d}");
    assert!(d.trim_end().ends_with("--failures | --item NAME | --lines A-B | --grep RE"), "{d}");
    // Without an exit code the verdict says it is inferred and the tail stays verbatim.
    let d = digest::digest("h:1", "cargo test", Shape::CargoTest, &text, None, 600);
    assert!(d.contains("exit status unknown (inferred: failed)"), "{d}");
    assert!(d.contains("error: test failed, to rerun pass `--lib`"), "{d}");
}

#[test]
fn a_failing_build_digests_to_its_errors() {
    let text = fixture("cargo-build-fail.txt");
    let d = digest::digest("h:2", "cargo build", Shape::CargoBuild, &text, Some(101), 600);
    assert!(d.contains("2 errors, 1 warnings: failed"), "{d}");
    assert!(d.contains("error:1 error[E0425] src/lib.rs:1 cannot find value `missing_name` in this scope"), "{d}");
    assert!(d.contains("error:2 error[E0308] src/lib.rs:2 mismatched types"), "{d}");
    assert!(d.contains("warnings: 1x unused variable: _"), "{d}");
    // error:2 carries its help section, which follows a blank line.
    let items = digest::items(Shape::CargoBuild, &text);
    let e2 = items.iter().find(|i| i.name == "error:2").unwrap();
    let block: Vec<&str> = text.lines().collect::<Vec<_>>()[e2.first - 1..e2.last].to_vec();
    assert!(block.iter().any(|l| l.starts_with("help: try using a conversion method")), "{block:?}");
}

#[test]
fn handles_store_once_and_slice_by_item_and_failures() {
    let t = Temp::new("handles");
    let mut h = store(&t);
    let text = fixture("cargo-test-fail.txt");
    let m = Meta { kind: "run".into(), label: "cargo test".into(), cmd: "cargo test".into(), exit_code: Some(101), ..Meta::default() };
    let a = h.put(text.as_bytes(), &m).unwrap();
    let b = h.put(text.as_bytes(), &m).unwrap();
    assert_eq!(a.id, b.id, "the same output is the same handle");
    assert!(a.id.starts_with("h:") && a.id.len() == 12, "{}", a.id);
    let f = h.slice(&a.id, &Sel::Failures).unwrap();
    assert!(f.contains("---- tests::panics_here stdout ----") && f.contains("right: 4"), "{f}");
    let one = h.slice(&a.id, &Sel::Item("adds_wrong".into())).unwrap();
    assert!(one.contains("assertion `left == right` failed") && !one.contains("widget"), "{one}");
    let lines = h.slice(&a.id, &Sel::Lines(17, 18)).unwrap();
    assert_eq!(lines, "17\ttest tests::adds ... ok\n18\ttest tests::panics_here ... FAILED\n");
    let g = h.slice(&a.id, &Sel::Grep { re: "FAILED".into(), context: 0 }).unwrap();
    assert_eq!(g.lines().count(), 4, "{g}");
    let missing = h.slice(&a.id, &Sel::Item("nope".into())).unwrap();
    assert!(missing.contains("no item \"nope\"") && missing.contains("tests::adds"), "{missing}");
    assert!(h.slice("h:ffffffffff", &Sel::Head(3)).is_err());
}

/// A grep slice whose backtracking state would pass the regex budget on a
/// long line is refused with the line and what to narrow, not answered
/// with nothing; an ordinary grep of the same output still works.
#[test]
fn a_grep_slice_over_the_regex_budget_is_refused() {
    let t = Temp::new("handles-grep-budget");
    let mut h = store(&t);
    let text = format!("first line\n{}\nlast line\n", "a".repeat(1 << 20));
    let a = h.put(text.as_bytes(), &Meta { kind: "run".into(), label: "wide".into(), ..Meta::default() }).unwrap();
    let e = h.slice(&a.id, &Sel::Grep { re: "(?:[a-z]\\d|\\d[a-z]){600}".into(), context: 0 }).unwrap_err().to_string();
    assert!(e.contains("line 2") && e.contains("narrow the pattern"), "{e}");
    let g = h.slice(&a.id, &Sel::Grep { re: "^(?:first|last) line$".into(), context: 0 }).unwrap();
    assert_eq!(g, "1\tfirst line\n--\n3\tlast line\n");
    assert_eq!(h.slice(&a.id, &Sel::Head(1)).unwrap(), "1\tfirst line\n");
}

#[test]
fn a_source_outline_slices_one_item() {
    let t = Temp::new("outline");
    let mut h = store(&t);
    let mut src = String::from("//! A module.\n\nuse std::fmt;\n\npub struct Widget {\n    count: u32,\n}\n\nimpl Widget {\n");
    for i in 0..40 {
        src.push_str(&format!("    pub fn step_{i}(&mut self) -> u32 {{\n        self.count += {i};\n        self.count\n    }}\n\n"));
    }
    src.push_str("}\n");
    let row = h.put(src.as_bytes(), &Meta { kind: "read".into(), label: "src/widget.rs".into(), cmd: "src/widget.rs".into(), ..Meta::default() }).unwrap();
    let d = h.digest(&row.id).unwrap();
    assert!(d.contains("outline, 42 items"), "{d}");
    assert!(d.contains("5-7 struct Widget"), "{d}");
    let s = h.slice(&row.id, &Sel::Item("Widget::step_3".into())).unwrap();
    assert_eq!(s.lines().count(), 4, "{s}");
    assert!(s.contains("self.count += 3;"), "{s}");
}

#[test]
fn eviction_drops_the_least_recently_used_beyond_the_cap() {
    let t = Temp::new("evict");
    let cfg = HandleCfg { cap_bytes: 3000, ..HandleCfg::default() };
    let mut h = Handles::open(&t.path().join("index.sqlite"), cfg).unwrap();
    let mut ids = Vec::new();
    for i in 0..4 {
        let body = format!("{i}").repeat(1000);
        ids.push(h.put(body.as_bytes(), &Meta { kind: "run".into(), cmd: format!("c{i}"), ..Meta::default() }).unwrap().id);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(h.row(&ids[0]).is_err(), "the oldest went");
    assert!(h.row(&ids[3]).is_ok());
}

fn bash_event(cmd: &str, stdout: &str) -> HookEvent {
    HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Bash".into(),
        session_id: "s1".into(),
        tool_input: json!({"command": cmd}),
        tool_response: json!({"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}),
        ..HookEvent::default()
    }
}

#[test]
fn routing_swaps_large_results_and_keeps_the_shape() {
    let t = Temp::new("route");
    let db = t.path().join("index.sqlite");
    let mut r = Router::with_db(&db, None);
    // Small: passes through.
    assert_eq!(r.post_tool_use(&bash_event("ls", "a\nb\n")).unwrap(), None);
    // Large: a digest plus handle in Bash's own shape.
    let text = fixture("cargo-test-fail.txt").repeat(40);
    let out = r.post_tool_use(&bash_event("cargo test", &text)).unwrap().expect("swapped");
    let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["stdout", "stderr", "interrupted", "isImage", "noOutputExpected"]);
    let stdout = out["stdout"].as_str().unwrap();
    assert!(stdout.starts_with("h:") && stdout.contains("exit status unknown (inferred: failed)") && stdout.contains("atlas slice h:"), "{stdout}");
    // Deterministic: the same event gives the same answer.
    assert_eq!(r.post_tool_use(&bash_event("cargo test", &text)).unwrap(), Some(out.clone()));
    // The escape mark and the slice command pass through.
    assert_eq!(r.post_tool_use(&bash_event("ATLAS_RAW=1 cargo test", &text)).unwrap(), None);
    assert_eq!(r.post_tool_use(&bash_event("atlas slice h:1 --lines 1-900", &text)).unwrap(), None);
    // Routing off: everything passes.
    set_routing(&db, false).unwrap();
    assert_eq!(r.post_tool_use(&bash_event("cargo test", &text)).unwrap(), None);
    set_routing(&db, true).unwrap();
    assert!(r.post_tool_use(&bash_event("cargo test", &text)).unwrap().is_some());
    // The configuration's switch.
    let mut off = Router::with_db(&db, Some(json!({"routing": false})));
    assert_eq!(off.post_tool_use(&bash_event("cargo test", &text)).unwrap(), None);
}

/// Real session finding: a Grep over the threshold on one file prints `N:text`
/// with no path, and the digest said `0 hits in 0 files`, which the model believed.
#[test]
fn a_grep_of_one_file_is_never_digested_as_no_hits() {
    let t = Temp::new("grep-one");
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    let numbered: String = (1..=800).map(|i| format!("{i}:alpha line {i} of a long file\n")).collect();
    let plain: String = (1..=800).map(|i| format!("alpha line {i} of a long file\n")).collect();
    let digits: String = (1..=800).map(|i| format!("{i}:5:alpha line {i} of a long file\n")).collect();
    let dated: String = (1..=800).map(|i| format!("{i}:2026-11-06 alpha line {i} of a long file\n")).collect();
    let stamped: String = (1..=800).map(|i| format!("2026-11-06 12:00:00 alpha line {i} of a long file\n")).collect();
    let tool = |content: &str| HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Grep".into(),
        session_id: "s1".into(),
        tool_input: json!({"pattern": "alpha", "path": "one.txt"}),
        tool_response: json!({"mode": "content", "content": content}),
        ..HookEvent::default()
    };
    for (name, ev) in [
        ("Grep tool", tool(&numbered)),
        ("Grep tool, no line numbers", tool(&plain)),
        ("Grep tool, digits after the line number", tool(&digits)),
        ("Grep tool, a date after the line number", tool(&dated)),
        ("Grep tool, a date first", tool(&stamped)),
        ("grep -n", bash_event("grep -n alpha one.txt", &numbered)),
        ("grep", bash_event("grep alpha one.txt", &plain)),
        ("rg -N", bash_event("rg -N alpha one.txt", &plain)),
    ] {
        let out = r.post_tool_use(&ev).unwrap().unwrap();
        let o = if name.starts_with("Grep") { out["content"].as_str().unwrap().to_string() } else { out["stdout"].as_str().unwrap().to_string() };
        assert!(!o.contains("0 hits"), "{name}: {o}");
        assert!(o.contains("800 lines") && !o.contains(" files"), "{name}: the count is there: {o}");
        // Bash output naming no file takes the log shape: its count and tail are honest.
        if name.starts_with("Grep") {
            assert!(o.contains("alpha line 1 "), "{name}: the first match is quoted: {o}");
        }
    }
    // Output that does name its files reads as before, and says so of any line that does not.
    let many: String = (1..=800).map(|i| format!("a{}.rs:{i}:alpha line\n", i % 2)).collect();
    let o = r.post_tool_use(&tool(&many)).unwrap().unwrap();
    assert!(o["content"].as_str().unwrap().contains("800 hits in 2 files"), "{o:?}");
    let d = digest::digest("h:1", "grep", Shape::Grep, "a.rs:1:x\nb.rs:2:y\nloose line\n", Some(0), 600);
    assert!(d.contains("2 hits in 2 files") && d.contains("and 1 lines naming no file"), "{d}");
}

#[test]
fn routing_reads_a_persisted_bash_output_whole() {
    let t = Temp::new("persist");
    let db = t.path().join("index.sqlite");
    let full: String = (1..=5000).map(|i| format!("{i}\n")).collect();
    let file = t.path().join("persisted.txt");
    std::fs::write(&file, &full).unwrap();
    let mut ev = bash_event("seq 1 5000", &full[..3000]);
    let o = ev.tool_response.as_object_mut().unwrap();
    o.insert("persistedOutputPath".into(), Value::from(file.display().to_string()));
    o.insert("persistedOutputSize".into(), Value::from(full.len() as u64));
    let mut r = Router::with_db(&db, None);
    let out = r.post_tool_use(&ev).unwrap().unwrap();
    assert!(out.get("persistedOutputPath").is_none());
    let stdout = out["stdout"].as_str().unwrap();
    assert!(stdout.contains("5000 lines"), "{stdout}");
    let id = stdout.split_whitespace().next().unwrap();
    let h = store(&t);
    assert_eq!(h.slice(id, &Sel::Tail(1)).unwrap(), "5000\t5000\n");
}

#[test]
fn routing_swaps_read_grep_and_glob_in_their_shapes() {
    let t = Temp::new("tools");
    let db = t.path().join("index.sqlite");
    let mut r = Router::with_db(&db, None);
    let body: String = (0..1000).map(|i| format!("fn function_number_{i}() {{}}\n")).collect();
    let read = HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Read".into(),
        tool_input: json!({"file_path": "/w/a.rs"}),
        tool_response: json!({"type": "text", "file": {"filePath": "/w/a.rs", "content": body, "numLines": 1000, "startLine": 1, "totalLines": 1000}}),
        ..HookEvent::default()
    };
    let out = r.post_tool_use(&read).unwrap().unwrap();
    let f = &out["file"];
    assert_eq!(f["filePath"], json!("/w/a.rs"));
    assert_eq!(f["totalLines"], json!(1000));
    assert!(f["content"].as_str().unwrap().contains("outline, 1000 items"));
    // A Read with its own window under three times the threshold passes.
    let mut windowed = read.clone();
    windowed.tool_input = json!({"file_path": "/w/a.rs", "offset": 1, "limit": 1000});
    assert_eq!(r.post_tool_use(&windowed).unwrap(), None);
    let hits: String = (0..900).map(|i| format!("src/m{}.rs:{i}:let hit = {i};\n", i % 7)).collect();
    let grep = HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Grep".into(),
        tool_input: json!({"pattern": "hit", "output_mode": "content"}),
        tool_response: json!({"mode": "content", "numFiles": 0, "filenames": [], "content": hits, "numLines": 900, "totalLines": 900}),
        ..HookEvent::default()
    };
    let out = r.post_tool_use(&grep).unwrap().unwrap();
    assert!(out["content"].as_str().unwrap().contains("900 hits in 7 files"), "{}", out["content"]);
    let names: Vec<Value> = (0..2000).map(|i| Value::from(format!("src/deep/folder/file{i}.rs"))).collect();
    let glob = HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Glob".into(),
        tool_input: json!({"pattern": "**/*.rs"}),
        tool_response: json!({"filenames": names, "durationMs": 5, "numFiles": 2000, "truncated": false, "totalMatches": 2000, "countIsComplete": true}),
        ..HookEvent::default()
    };
    let out = r.post_tool_use(&glob).unwrap().unwrap();
    let first = out["filenames"][0].as_str().unwrap();
    assert!(first.starts_with("h:") && first.contains("Glob"), "{first}");
    assert_eq!(out["numFiles"], json!(2000));
}

#[test]
fn atlas_run_prints_small_output_whole_and_large_output_as_a_digest() {
    let t = Temp::new("run");
    let cfg = t.path().join("atlas.json");
    let db = t.path().join("index.sqlite");
    std::fs::write(&cfg, json!({"db": db.display().to_string(), "vaults": []}).to_string()).unwrap();
    let big = t.path().join("big.txt");
    let text: String = (0..3000).map(|i| format!("line {i} of a long output\n")).collect();
    std::fs::write(&big, &text).unwrap();
    let show: Vec<String> = if cfg!(windows) {
        vec!["cmd".into(), "/C".into(), "type".into(), big.display().to_string()]
    } else {
        vec!["cat".into(), big.display().to_string()]
    };
    let run = |args: &[String]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_atlas")).arg("run").arg("--").args(args).env("ATLAS_CONFIG", &cfg).current_dir(t.path()).output().unwrap()
    };
    let out = run(&show);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("h:") && s.contains("exit 0") && s.contains("3000 lines"), "{s}");
    assert!(s.contains("atlas slice h:"), "{s}");
    let small: Vec<String> = if cfg!(windows) { vec!["cmd".into(), "/C".into(), "echo hi".into()] } else { vec!["echo".into(), "hi".into()] };
    let out = run(&small);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    let fail: Vec<String> = if cfg!(windows) { vec!["cmd".into(), "/C".into(), "exit 3".into()] } else { vec!["sh".into(), "-c".into(), "exit 3".into()] };
    assert_eq!(run(&fail).status.code(), Some(3), "the child's exit code");
}

#[test]
fn shell_reads_and_builds_get_an_exit_tail_and_nothing_else_does() {
    assert!(routable("cargo test -j 8"));
    assert!(routable("cd /w && RUST_LOG=1 cargo +nightly clippy --all"));
    assert!(routable("sed -n '10,80p' src/a.rs"));
    assert!(routable("Get-Content a.txt"));
    assert!(!routable("cargo fmt"));
    assert!(!routable("npm install"));
    assert_eq!(rewrite("Bash", "cargo test"), Some(format!("cargo test || echo {EXIT_MARK}")));
    assert_eq!(rewrite("Bash", "grep -n x a.rs # why"), None, "a comment would swallow the tail");
    assert_eq!(rewrite("Bash", "cat a\ncat b"), None, "several lines run untailed");
    assert_eq!(rewrite("PowerShell", "Get-Content a.txt"), Some("Get-Content a.txt\nWrite-Output ''".to_string()));
    // A PowerShell cargo build or test gets the exit reset on Windows; its status reads as inferred.
    let ps = rewrite("PowerShell", "cargo test -j 8").unwrap();
    if cfg!(windows) {
        assert_eq!(ps, format!("cargo test -j 8\n{}", atlas::hook_rewrite::PS_RESET));
    } else {
        assert_eq!(ps, "cargo test -j 8\nWrite-Output ''");
    }
    assert_eq!(unwrite(&ps, "test result: ok."), ("cargo test -j 8", None));
    let r = rewrite("Bash", "cargo test").unwrap();
    assert_eq!(unwrite(&r, &format!("out\n{EXIT_MARK}\n")), ("cargo test", Some(-1)));
    assert_eq!(unwrite(&r, "out\n"), ("cargo test", Some(0)));
    assert_eq!(unwrite("cat a\ntrue", ""), ("cat a", None));
    assert_eq!(relative(r"C:\w\src\a.rs", r"C:\w"), "src/a.rs");
    assert_eq!(relative("/x/a.rs", "/w"), "/x/a.rs");

    let t = Temp::new("pre");
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    let pre = |cmd: &str, bg: bool| HookEvent {
        event: "PreToolUse".into(),
        tool_name: "Bash".into(),
        tool_input: json!({"command": cmd, "description": "d", "timeout": 600000, "run_in_background": bg}),
        ..HookEvent::default()
    };
    let out = r.pre_tool_use(&pre("cargo test", false)).unwrap().unwrap();
    assert_eq!(out, json!({"command": format!("cargo test || echo {EXIT_MARK}"), "description": "d", "timeout": 600000, "run_in_background": false}));
    assert_eq!(r.pre_tool_use(&pre("cargo test", true)).unwrap(), None, "background commands are left alone");
    assert_eq!(r.pre_tool_use(&pre("npm install", false)).unwrap(), None);
    assert_eq!(r.pre_tool_use(&pre("ATLAS_RAW=1 cat a", false)).unwrap(), None);
}

/// Auto mode gets no exit tail (its classifier judges the command as
/// written, issue #67); every other mode, and an input with no mode, keeps it.
#[test]
fn the_exit_tail_follows_the_permission_mode() {
    let t = Temp::new("modes");
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    let pre = |tool: &str, cmd: &str, mode: &str| HookEvent {
        event: "PreToolUse".into(),
        tool_name: tool.into(),
        permission_mode: mode.into(),
        tool_input: json!({"command": cmd, "description": "d"}),
        ..HookEvent::default()
    };
    for cmd in ["git show HEAD", "sed -n 1,20p src/a.rs", "grep -n fn src/a.rs", "cargo test"] {
        assert_eq!(r.pre_tool_use(&pre("Bash", cmd, "auto")).unwrap(), None, "auto: {cmd}");
        assert_eq!(r.pre_tool_use(&pre("PowerShell", cmd, "auto")).unwrap(), None, "auto PowerShell: {cmd}");
        for mode in ["", "default", "plan", "acceptEdits", "dontAsk", "bypassPermissions"] {
            let out = r.pre_tool_use(&pre("Bash", cmd, mode)).unwrap().unwrap_or_else(|| panic!("{mode}: {cmd}"));
            assert_eq!(out["command"], json!(format!("{cmd} || echo {EXIT_MARK}")), "{mode}: {cmd}");
            assert_eq!(out["description"], json!("d"));
        }
    }
    // ATLAS_RAW=1 skips the tail in every mode.
    for mode in ["", "default", "auto", "bypassPermissions"] {
        for cmd in ["ATLAS_RAW=1 git show HEAD", "ATLAS_RAW=1 cargo test", "cd x && ATLAS_RAW=1 grep -n a b"] {
            assert_eq!(r.pre_tool_use(&pre("Bash", cmd, mode)).unwrap(), None, "{mode}: {cmd}");
            assert_eq!(r.pre_tool_use(&pre("PowerShell", cmd, mode)).unwrap(), None, "{mode} PowerShell: {cmd}");
        }
    }
    // The mode reaches the router from Claude Code's hook input, and from an
    // mcp_tool hook that had no value to substitute.
    let v = json!({"hook_event_name": "PreToolUse", "permission_mode": "auto", "tool_name": "Bash", "tool_input": {"command": "cat a"}});
    let ev = HookEvent::from_value(&v);
    assert_eq!(ev.permission_mode, "auto");
    assert_eq!(r.pre_tool_use(&ev).unwrap(), None);
    let v = json!({"event": "PreToolUse", "permission_mode": "${permission_mode}", "tool_name": "Bash", "tool_input": "{\"command\": \"cat a\"}"});
    let ev = HookEvent::from_value(&v);
    assert_eq!(ev.permission_mode, "");
    assert_eq!(r.pre_tool_use(&ev).unwrap().unwrap()["command"], json!(format!("cat a || echo {EXIT_MARK}")));
}

/// In auto mode the command runs as written: a success is still digested
/// with no exit marker to strip.
#[test]
fn an_untailed_success_is_still_digested() {
    let t = Temp::new("untailed");
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    let text = fixture("cargo-test-fail.txt").repeat(40);
    let out = r.post_tool_use(&bash_event("cargo test", &text)).unwrap().expect("a large untailed result is routed");
    let stdout = out["stdout"].as_str().unwrap();
    assert!(stdout.len() < text.len(), "digested");
    assert!(!stdout.contains(EXIT_MARK));
}

#[test]
fn a_failed_rewritten_command_says_so_small_or_large() {
    let t = Temp::new("fail");
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    let cmd = rewrite("Bash", "cargo test").unwrap();
    // The marker counts only as the exact final line.
    let quoted = bash_event(&cmd, &format!("the output names {EXIT_MARK} here"));
    assert_eq!(r.post_tool_use(&quoted).unwrap(), None, "a success that mentions the marker passes untouched");
    let own = bash_event(&cmd, &format!("line\nnot {EXIT_MARK}"));
    assert_eq!(r.post_tool_use(&own).unwrap(), None);
    let small = bash_event(&cmd, &format!("error: nope\n{EXIT_MARK}"));
    let out = r.post_tool_use(&small).unwrap().unwrap();
    assert_eq!(out["stdout"], json!("Exit code nonzero\nerror: nope"));
    let text = fixture("cargo-test-fail.txt").repeat(40);
    let big = bash_event(&cmd, &format!("{text}{EXIT_MARK}\n"));
    let out = r.post_tool_use(&big).unwrap().unwrap();
    let d = out["stdout"].as_str().unwrap();
    assert!(d.contains("cargo test exit nonzero,") && d.contains("): failed\n"), "{d}");
    assert!(!d.contains(EXIT_MARK), "{d}");
}

/// Only git's reads are routed: a write keeps its own exit status, whatever
/// global options come first or wherever it sits in a chain (issue
/// bloodfeast/atlas#3).
#[test]
fn git_reads_are_routed_and_git_writes_keep_their_exit_status() {
    for cmd in [
        "git log --oneline -5",
        "git diff HEAD~1",
        "git show HEAD:src/a.rs",
        "git status --short",
        "git blame -L 1,20 src/a.rs",
        "git grep -n fn",
        "git ls-files src",
        "git rev-parse HEAD",
        "git shortlog -sn",
        "git describe --tags",
        "git cat-file -p HEAD",
        "git ls-tree HEAD",
        "git -C H:/w log -3",
        "git -c core.pager=cat diff",
        "git --no-pager log -1",
        "git --git-dir=.git status",
        "/usr/bin/git.exe status",
        "cd /w && git log -1",
        "cd /w && GIT_PAGER=cat git --no-pager -C x show HEAD",
        "git log -1 | head -5",
        "git status && git diff --stat",
        "git -C \"a b\" status",
        "cat `git ls-files | head -1`",
    ] {
        assert!(routable(cmd), "{cmd}");
    }
    for cmd in [
        "git push",
        "git push origin main",
        "git commit -m 'x'",
        "git merge main",
        "git rebase origin/main",
        "git tag v1",
        "git reset --hard",
        "git checkout main",
        "git switch -c b",
        "git stash",
        "git pull",
        "git fetch",
        "git add .",
        "git rm a",
        "git mv a b",
        "git branch -d b",
        "git lg",
        "git",
        "git --no-pager",
        "git -C H:/w push",
        "git -C log push",
        "git -c user.name=x commit -m y",
        "git --no-pager -c k=v merge x",
        "git --git-dir .git push",
        "cd /w && git push",
        "cd /w && GIT_TRACE=1 git commit -m x",
        "git push && gh pr create",
        "git status && git push",
        "cat a && git commit -m x",
        "git log -1; git tag v2",
        "ls || git push",
        "(git fetch) && git log",
        // git that is not the first word of its step.
        "cat l | xargs git add",
        "ls && sudo git push",
        "ls && time git push",
        "ls; env git push",
        "cat `git push`",
        "cat $(git commit -m x)",
        "ls && ! git push",
        "ls && !git push",
        "ls && bash -c \"git push\"",
        "ls && sh -c 'git -C x merge y'",
        "git --attr-source log push",
        "git --shallow-file log push",
        r"git -C a\ log push",
        // An option value never eats an inner command or a quoted word.
        "git -C \"$(git rev-parse --show-toplevel)\" push origin HEAD",
        "git -C \"a log\" push",
        "cargo test && git push",
    ] {
        assert!(!routable(cmd), "{cmd}");
    }
    // A git write gets no tail in any mode.
    let t = Temp::new("gitw");
    let mut r = Router::with_db(&t.path().join("index.sqlite"), None);
    for mode in ["", "default", "bypassPermissions"] {
        let ev = HookEvent {
            event: "PreToolUse".into(),
            tool_name: "Bash".into(),
            permission_mode: mode.into(),
            tool_input: json!({"command": "git push && gh pr create", "description": "d"}),
            ..HookEvent::default()
        };
        assert_eq!(r.pre_tool_use(&ev).unwrap(), None, "{mode}");
    }
}
