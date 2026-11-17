//! Read routing (`lean.route_read`, journal:ATLAS-HOOK-VALUE:0012): which
//! whole-file Reads become a digest in each mode, the ranged and
//! already-read passes, and the ledger row a routed Read leaves.

mod common;

use atlas::hook_rewrite::Router;
use atlas::hooks::HookEvent;
use atlas::json;
use atlas::json::Value;
use common::Temp;

const SRC: &str = "C:/w/src/big.rs";
const WIN_OUT: &str = "C:\\Users\\u\\.claude\\projects\\p\\0f1e\\tool-results\\toolu_01.txt";
const POSIX_TASK: &str = "/tmp/claude-1000/p/0f1e/tasks/b7x2.output";

/// `n` bytes of text: lines of 50 bytes, the last one cut to fit.
fn text(n: usize) -> String {
    let line = format!("{}\n", "x".repeat(49));
    let mut t = line.repeat(n / 50 + 1);
    t.truncate(n);
    t
}

fn read(session: &str, path: &str, content: &str, input: Value) -> HookEvent {
    let lines = content.lines().count() as u64;
    let mut ti = input;
    ti.as_object_mut().unwrap().insert("file_path".into(), Value::from(path));
    HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Read".into(),
        session_id: session.into(),
        tool_input: ti,
        tool_response: json!({"type": "text", "file": {"filePath": path, "content": content, "numLines": lines, "startLine": 1, "totalLines": lines}}),
        ..HookEvent::default()
    }
}

fn whole(path: &str, n: usize) -> HookEvent {
    read("s1", path, &text(n), json!({}))
}

fn router(t: &Temp, lean: Option<Value>) -> Router {
    Router::with_db(&t.path().join("index.sqlite"), lean)
}

fn routed(r: &mut Router, ev: &HookEvent) -> bool {
    match r.post_tool_use(ev).unwrap() {
        None => false,
        Some(out) => {
            let c = out["file"]["content"].as_str().unwrap();
            assert!(c.starts_with("h:"), "a digest: {c}");
            true
        }
    }
}

fn mode(m: Value) -> Option<Value> {
    Some(json!({"route_read": m}))
}

#[test]
fn outputs_is_the_default_and_digests_only_tool_output_files() {
    for lean in [None, mode(json!("outputs")), mode(json!("nonsense")), Some(json!({}))] {
        let t = Temp::new("rr-outputs");
        let mut r = router(&t, lean.clone());
        assert!(!routed(&mut r, &whole(SRC, 64 << 10)), "{lean:?}: a source file passes whole");
        assert!(!routed(&mut r, &whole("/w/docs/BRIEF.md", 40_000)), "{lean:?}: a brief passes whole");
        assert!(routed(&mut r, &whole(WIN_OUT, 40_000)), "{lean:?}: a Windows tool-results file");
        assert!(routed(&mut r, &whole(POSIX_TASK, 40_000)), "{lean:?}: a POSIX task output");
    }
}

#[test]
fn tool_output_paths_are_recognised_on_both_separators() {
    let t = Temp::new("rr-paths");
    let mut r = router(&t, None);
    let cases = [
        ("/home/u/.claude/projects/p/s/tool-results/b1.txt", true),
        ("C:/Users/u/.claude/projects/p/s/tool-results/b2.txt", true),
        ("C:\\Users\\u\\AppData\\Local\\Temp\\claude\\p\\s\\tasks\\a9.output", true),
        ("/tmp/claude/p/s/tasks/a9.output", true),
        ("/tmp/claude/p/s/TASKS/A8.OUTPUT", true),
        // A tasks folder without an .output file, an .output file elsewhere,
        // and a name that only looks like the folder.
        ("/tmp/claude/p/s/tasks/notes.txt", false),
        ("/w/build/run.output", false),
        ("/w/my-tool-results/b.txt", false),
        ("C:\\w\\src\\tool_results.rs", false),
    ];
    for (p, want) in cases {
        assert_eq!(routed(&mut r, &whole(p, 30_000)), want, "{p}");
    }
}

#[test]
fn outputs_digests_from_twelve_thousand_bytes() {
    let t = Temp::new("rr-edge");
    let mut r = router(&t, None);
    assert!(!routed(&mut r, &whole(WIN_OUT, 11_999)));
    assert!(routed(&mut r, &whole(POSIX_TASK, 12_000)));
}

#[test]
fn off_passes_everything() {
    let t = Temp::new("rr-off");
    let mut r = router(&t, mode(json!("off")));
    assert!(!routed(&mut r, &whole(SRC, 200_000)));
    assert!(!routed(&mut r, &whole(WIN_OUT, 200_000)));
}

#[test]
fn tuned_digests_from_route_read_min_kb() {
    let t = Temp::new("rr-tuned");
    let mut r = router(&t, mode(json!("tuned")));
    assert!(!routed(&mut r, &whole(SRC, 32 * 1024 - 1)));
    assert!(routed(&mut r, &whole("C:/w/src/other.rs", 32 * 1024)));
    // Tool outputs get no special treatment in tuned.
    assert!(!routed(&mut r, &whole(WIN_OUT, 20_000)));
    let t = Temp::new("rr-tuned-16");
    let mut r = router(&t, Some(json!({"route_read": "tuned", "route_read_min_kb": 16})));
    assert!(!routed(&mut r, &whole(SRC, 16 * 1024 - 1)));
    assert!(routed(&mut r, &whole("C:/w/src/other.rs", 16 * 1024)));
}

#[test]
fn route_read_min_kb_does_not_touch_the_other_modes() {
    let t = Temp::new("rr-minkb");
    let mut r = router(&t, Some(json!({"route_read": "all", "route_read_min_kb": 64})));
    assert!(routed(&mut r, &whole(SRC, 13_000)));
    let t = Temp::new("rr-minkb-out");
    let mut r = router(&t, Some(json!({"route_read_min_kb": 64})));
    assert!(routed(&mut r, &whole(WIN_OUT, 13_000)));
}

#[test]
fn all_digests_every_file_over_the_threshold() {
    let t = Temp::new("rr-all");
    let mut r = router(&t, mode(json!("all")));
    assert!(!routed(&mut r, &whole(SRC, 11_999)));
    assert!(routed(&mut r, &whole(SRC, 12_000)));
    assert!(routed(&mut r, &whole("/w/docs/BRIEF.md", 20_000)));
}

#[test]
fn the_old_boolean_maps_true_to_tuned_and_false_to_off() {
    let t = Temp::new("rr-bool-true");
    let mut r = router(&t, mode(json!(true)));
    assert!(!routed(&mut r, &whole(SRC, 20_000)), "true is tuned: under 32 KB passes");
    assert!(routed(&mut r, &whole("C:/w/src/other.rs", 40_000)), "true is tuned: 32 KB and up is digested");
    assert!(!routed(&mut r, &whole(WIN_OUT, 20_000)), "true is tuned, not outputs");
    let t = Temp::new("rr-bool-false");
    let mut r = router(&t, mode(json!(false)));
    assert!(!routed(&mut r, &whole(SRC, 200_000)));
    assert!(!routed(&mut r, &whole(WIN_OUT, 200_000)));
}

#[test]
fn a_ranged_read_always_passes() {
    for m in ["outputs", "tuned", "all"] {
        let t = Temp::new("rr-ranged");
        let mut r = router(&t, mode(json!(m)));
        for input in [json!({"offset": 10}), json!({"limit": 2000}), json!({"offset": 1, "limit": 5000})] {
            for p in [SRC, WIN_OUT, POSIX_TASK] {
                assert!(!routed(&mut r, &read("s1", p, &text(200_000), input.clone())), "{m} {p} {input:?}");
            }
        }
    }
}

#[test]
fn a_path_already_read_in_this_session_passes_whole() {
    for (m, p) in [("all", SRC), ("tuned", SRC), ("outputs", WIN_OUT), ("outputs", POSIX_TASK)] {
        let t = Temp::new("rr-again");
        let mut r = router(&t, mode(json!(m)));
        let body = text(60_000);
        assert!(routed(&mut r, &read("s1", p, &body, json!({}))), "{m}: first Read");
        assert!(!routed(&mut r, &read("s1", p, &body, json!({}))), "{m}: coming back for the text");
        // Another session, or another subagent of the same one, has no handle yet.
        assert!(routed(&mut r, &read("s2", p, &body, json!({}))), "{m}: another session");
        let mut sub = read("s1", p, &body, json!({}));
        sub.agent_id = "agent-a".into();
        assert!(routed(&mut r, &sub), "{m}: a subagent");
        assert!(!routed(&mut r, &sub), "{m}: the subagent coming back");
    }
}

/// E4 (journal:ATLAS-EFFICIENCY:0006): a routed Read logs its session, the
/// transcript that holds it (a subagent's own), the file and the tool call,
/// so the report can subtract later Reads of the file (judged credit).
#[test]
fn a_routed_read_logs_where_it_was_read_for_the_judged_credit() {
    let t = Temp::new("rr-where");
    let db = t.path().join("index.sqlite");
    let mut r = Router::with_db(&db, None);
    let parent = "C:/p/sess.jsonl";
    let mut main = whole(WIN_OUT, 40_000);
    main.transcript_path = parent.into();
    main.tool_use_id = "toolu_main".into();
    assert!(routed(&mut r, &main));
    let mut sub = whole(POSIX_TASK, 30_000);
    sub.transcript_path = parent.into();
    sub.agent_id = "a7".into();
    sub.tool_use_id = "toolu_sub".into();
    assert!(routed(&mut r, &sub));
    let conn = rusqlite::Connection::open(&db).unwrap();
    type Row = (String, String, String, String, Option<i64>, Option<i64>);
    let rows: Vec<Row> = conn
        .prepare("SELECT session, transcript, path, tool_use, grep, read FROM token_log WHERE tool = 'route:Read' ORDER BY id")
        .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect())
        .unwrap();
    let sub_transcript = atlas::hooks::subagent_transcript(parent, "a7").to_string_lossy().into_owned();
    assert_eq!(
        rows,
        vec![
            ("s1".into(), parent.into(), WIN_OUT.into(), "toolu_main".into(), None, Some(40_000)),
            ("s1".into(), sub_transcript, POSIX_TASK.into(), "toolu_sub".into(), None, Some(30_000)),
        ]
    );
}

#[test]
fn a_routed_read_is_logged_without_a_counterfactual() {
    let t = Temp::new("rr-ledger");
    let db = t.path().join("index.sqlite");
    let mut r = Router::with_db(&db, mode(json!("all")));
    assert!(routed(&mut r, &whole(SRC, 40_000)));
    let bash = HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Bash".into(),
        session_id: "s1".into(),
        tool_input: json!({"command": "cargo build"}),
        tool_response: json!({"stdout": text(40_000), "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}),
        ..HookEvent::default()
    };
    assert!(r.post_tool_use(&bash).unwrap().is_some());
    let conn = rusqlite::Connection::open(&db).unwrap();
    let rows: Vec<(String, i64, Option<i64>, Option<i64>)> = conn
        .prepare("SELECT tool, returned, grep, read FROM token_log ORDER BY id")
        .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect())
        .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let (tool, returned, grep, read) = &rows[0];
    assert_eq!(tool, "route:Read");
    assert!(*returned > 0);
    // The output it replaced is logged, but the ledger's SQL credit stays 0:
    // the report judges it against later Reads (E4, ATLAS-EFFICIENCY:0006).
    assert_eq!((*grep, *read), (None, Some(40_000)), "route:Read logs the output it replaced, grep NULL like a hook row");
    let (by_tool, _) = atlas::tokens::report(&conn, 1.0).unwrap();
    assert_eq!(by_tool.iter().find(|r| r.key == "route:Read").unwrap().counterfactual, 0.0, "no credit in SQL");
    // The shell route keeps its counterfactual, capped at what Claude Code
    // shows of a Bash output: its first 30,000 characters (#215).
    assert_eq!(rows[1].0, "route:Bash");
    assert_eq!(rows[1].3, Some(30_000));
}

/// #215: a shell output over the cap credits exactly the bytes of its first
/// 30,000 characters, cut on a character boundary; one under it, in full.
#[test]
fn a_routed_shell_output_is_credited_up_to_what_claude_code_shows() {
    let t = Temp::new("rr-shell-cap");
    let db = t.path().join("index.sqlite");
    let mut r = Router::with_db(&db, mode(json!("all")));
    let bash = |out: String| HookEvent {
        event: "PostToolUse".into(),
        tool_name: "Bash".into(),
        session_id: "s1".into(),
        tool_input: json!({"command": "cargo build"}),
        tool_response: json!({"stdout": out, "stderr": "", "interrupted": false, "isImage": false, "noOutputExpected": false}),
        ..HookEvent::default()
    };
    // 29,999 one-byte characters, then a two-byte one as the 30,000th.
    let over = format!("{}\u{e9}{}", "a".repeat(29_999), "b\n".repeat(5_000));
    assert!(r.post_tool_use(&bash(over)).unwrap().is_some());
    let under = text(20_000);
    let n = under.len() as i64;
    assert!(r.post_tool_use(&bash(under)).unwrap().is_some());
    let conn = rusqlite::Connection::open(&db).unwrap();
    let reads: Vec<Option<i64>> = conn
        .prepare("SELECT read FROM token_log WHERE tool = 'route:Bash' ORDER BY id")
        .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect())
        .unwrap();
    assert_eq!(reads, [Some(29_999 + 2), Some(n)]);
}
