//! `atlas settings`: list, set, off, undo and offer, on a scratch home (the
//! real Claude Code folder and atlas home are never touched), through the
//! binary and through the library with scripted answers.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use atlas::claude_settings::{self as cs, Io, Paths};
use atlas::json;
use common::*;

/// A scratch home: atlas's folder and Claude Code's, plus HOME/USERPROFILE.
struct Home {
    t: Temp,
}

impl Home {
    fn new(tag: &str) -> Home {
        let t = Temp::new(tag);
        std::fs::create_dir_all(t.path().join("claude")).unwrap();
        Home { t }
    }
    fn atlas(&self) -> PathBuf {
        self.t.path().join("atlas")
    }
    fn claude(&self) -> PathBuf {
        self.t.path().join("claude")
    }
    fn settings(&self) -> PathBuf {
        self.claude().join("settings.json")
    }
    fn config(&self) -> PathBuf {
        self.atlas().join("atlas.json")
    }
    fn backups(&self) -> usize {
        std::fs::read_dir(self.claude().join("atlas-backups")).map(|r| r.count()).unwrap_or(0)
    }
    /// Runs `atlas settings ARGS` as a fresh user would: no ATLAS_CONFIG,
    /// the first run writes the template.
    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(bin())
            .arg("settings")
            .args(args)
            .env("ATLAS_HOME", self.atlas())
            .env("CLAUDE_CONFIG_DIR", self.claude())
            .env("HOME", self.t.path())
            .env("USERPROFILE", self.t.path())
            .env_remove("ATLAS_CONFIG")
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    }
    fn paths(&self) -> Paths {
        Paths { claude_dir: self.claude(), atlas_config: Some(self.config()), state: Some(self.atlas().join("state").join(cs::OFFER_STATE)), db: Some(self.atlas().join("state").join("atlas.sqlite")), user_state: None }
    }
    fn read(&self, p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }
}

/// The library entry with scripted answers and a given Claude Code version.
fn scripted(p: &Paths, args: &[&str], answers: &str, version: Option<(u32, u32, u32)>) -> (bool, String) {
    let mut out = Vec::new();
    let mut input = answers.as_bytes();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let code = cs::run(&args, p, &mut Io { out: &mut out, input: &mut input, interactive: true, version }).unwrap();
    (code == std::process::ExitCode::SUCCESS, String::from_utf8(out).unwrap())
}

#[test]
fn list_explains_every_setting_on_a_fresh_home() {
    let h = Home::new("cs-list");
    let (ok, out) = h.run(&["list"]);
    assert!(ok, "{out}");
    for id in ["subagent-cache-1h", "goal-checkin-120", "routing", "update", "update-pin"] {
        assert!(out.contains(&format!("[{id}]")), "{id}: {out}");
    }
    assert!(out.contains("What:") && out.contains("Cost:") && out.contains("Measured:") && out.contains("Docs:"), "{out}");
    assert!(out.contains("(not set)") && out.contains("(on by default)"), "{out}");
    // Nothing was written to Claude Code's folder; the first run wrote atlas's template.
    assert!(!h.settings().exists());
    assert!(h.config().is_file());
}

#[test]
fn offer_yes_applies_once_and_undo_removes_a_file_atlas_created() {
    let h = Home::new("cs-yes");
    let (ok, out) = h.run(&["offer", "--yes", "subagent-cache-1h"]);
    assert!(ok, "{out}");
    assert!(out.contains("+  \"subagentPromptCacheTtl\": \"1h\""), "the diff is shown: {out}");
    assert_eq!(h.read(&h.settings()), "{\n  \"subagentPromptCacheTtl\": \"1h\"\n}\n");
    assert_eq!(h.backups(), 1, "a backup recording that the file was absent");
    // Idempotent: reported, not rewritten.
    let (ok, out) = h.run(&["offer", "--yes", "subagent-cache-1h"]);
    assert!(ok && out.contains("[subagent-cache-1h] already applied") && out.contains("nothing to change"), "{out}");
    assert_eq!(h.backups(), 1);
    let (ok, out) = h.run(&["undo"]);
    assert!(ok && out.contains("did not exist"), "{out}");
    assert!(!h.settings().exists());
    let (ok, out) = h.run(&["undo"]);
    assert!(!ok && out.contains("no atlas backup"), "{out}");
    // An unknown id is refused before anything is written.
    let (ok, out) = h.run(&["offer", "--yes", "nope"]);
    assert!(!ok && out.contains("unknown id"), "{out}");
}

#[test]
fn set_off_and_undo_keep_every_other_key() {
    let h = Home::new("cs-set");
    // A settings file as Claude Code writes it: two-space JSON, other keys,
    // nested objects, non-ASCII text, no trailing newline.
    let original = "{\n  \"model\": \"opus\",\n  \"env\": {\n    \"KEEP\": \"1\"\n  },\n  \"hooks\": {\n    \"Stop\": []\n  },\n  \"note\": \"caf\u{e9}\"\n}";
    std::fs::write(h.settings(), original).unwrap();
    let (ok, out) = h.run(&["set", "goal-checkin-120", "0"]);
    assert!(ok, "{out}");
    assert!(out.contains("warning: 0 turns check-ins off"), "{out}");
    let v = json::parse(&h.read(&h.settings())).unwrap();
    assert_eq!(v["env"]["CLAUDE_CODE_GOAL_CHECKIN_MINUTES"], "0");
    assert_eq!(v["env"]["KEEP"], "1");
    assert_eq!(v["model"], "opus");
    assert_eq!(v["note"], "caf\u{e9}");
    assert!(!h.read(&h.settings()).ends_with('\n'), "the file's own ending is kept");
    let (ok, out) = h.run(&["set", "goal-checkin-120", "0"]);
    assert!(ok && out.contains("already"), "{out}");
    let (ok, out) = h.run(&["set", "goal-checkin-120", "9999"]);
    assert!(!ok && out.contains("0 to 1440"), "{out}");
    let (ok, out) = h.run(&["set", "subagent-cache-1h", "1h"]);
    assert!(ok, "{out}");
    let (ok, out) = h.run(&["off", "goal-checkin-120"]);
    assert!(ok && out.contains("Claude Code default: 30"), "{out}");
    let v = json::parse(&h.read(&h.settings())).unwrap();
    assert!(v["env"].get("CLAUDE_CODE_GOAL_CHECKIN_MINUTES").is_none());
    assert_eq!(v["env"]["KEEP"], "1", "env keeps its other keys");
    let (ok, out) = h.run(&["off", "goal-checkin-120"]);
    assert!(ok && out.contains("nothing to change"), "{out}");
    // Undo walks back one write at a time to the original bytes.
    assert_eq!(h.backups(), 3);
    for _ in 0..3 {
        let (ok, out) = h.run(&["undo"]);
        assert!(ok, "{out}");
    }
    assert_eq!(h.read(&h.settings()), original);
    assert_eq!(h.backups(), 0);
}

#[test]
fn an_env_object_atlas_empties_is_removed() {
    let h = Home::new("cs-env");
    let (ok, _) = h.run(&["set", "goal-checkin-120", "120"]);
    assert!(ok);
    let (ok, _) = h.run(&["off", "goal-checkin-120"]);
    assert!(ok);
    assert_eq!(h.read(&h.settings()), "{}\n");
}

#[test]
fn a_file_that_does_not_round_trip_is_never_rewritten() {
    let h = Home::new("cs-refuse");
    let compact = "{\"model\":\"opus\",\"env\":{}}\n";
    std::fs::write(h.settings(), compact).unwrap();
    let (ok, out) = h.run(&["set", "subagent-cache-1h", "1h"]);
    assert!(!ok, "{out}");
    assert!(out.contains("will not rewrite") && out.contains("set \"subagentPromptCacheTtl\": \"1h\" at the top level"), "{out}");
    assert_eq!(h.read(&h.settings()), compact);
    assert_eq!(h.backups(), 0);
    // CRLF line endings in the canonical layout do round-trip, and are kept.
    let crlf = "{\r\n  \"model\": \"opus\"\r\n}\r\n";
    std::fs::write(h.settings(), crlf).unwrap();
    let (ok, out) = h.run(&["set", "subagent-cache-1h", "1h"]);
    assert!(ok, "{out}");
    assert_eq!(h.read(&h.settings()), "{\r\n  \"model\": \"opus\",\r\n  \"subagentPromptCacheTtl\": \"1h\"\r\n}\r\n");
    // Not JSON at all: an error, nothing written.
    std::fs::write(h.settings(), "{nope").unwrap();
    let (ok, out) = h.run(&["set", "subagent-cache-1h", "5m"]);
    assert!(!ok && out.contains("not JSON"), "{out}");
}

#[test]
fn atlas_switches_change_only_their_value_in_atlas_config() {
    let h = Home::new("cs-atlas");
    let (ok, _) = h.run(&["list"]);
    assert!(ok);
    let template = h.read(&h.config());
    let db = h.atlas().join("state").join("atlas.sqlite");
    let lean = || atlas::config::Config::load(&h.config()).unwrap().lean;
    // Routing is a flag beside the index: the configuration is not touched.
    let (ok, out) = h.run(&["off", "routing"]);
    assert!(ok && out.contains("[routing] turned off"), "{out}");
    assert_eq!(h.read(&h.config()), template);
    assert!(!atlas::hook_rewrite::routing_on(&db, lean().as_ref()));
    let (ok, out) = h.run(&["off", "routing"]);
    assert!(ok && out.contains("already off"), "{out}");
    let (ok, out) = h.run(&["list"]);
    assert!(ok && out.contains("[routing] Route large tool results through atlas  (off)"), "{out}");
    let (ok, out) = h.run(&["set", "routing", "on"]);
    assert!(ok && out.contains("[routing] turned on"), "{out}");
    assert!(atlas::hook_rewrite::routing_on(&db, lean().as_ref()));
    // The updater's switches are spliced into update.* and nothing else.
    let (ok, out) = h.run(&["set", "update-pin", "0.1.0"]);
    assert!(ok, "{out}");
    assert!(out.contains("+  \"update\": {\"pin\": \"0.1.0\"}"), "{out}");
    let after = h.read(&h.config());
    let end = template.trim_end().strip_suffix('}').unwrap().trim_end();
    assert!(after.starts_with(end), "the template's text is kept byte for byte:\n{after}");
    let (ok, out) = h.run(&["set", "update", "off"]);
    assert!(ok, "{out}");
    let u = atlas::config::Config::load(&h.config()).unwrap().update.unwrap();
    assert_eq!((u.enabled, u.pin.as_deref()), (Some(false), Some("0.1.0")));
    let (ok, out) = h.run(&["off", "update-pin"]);
    assert!(ok, "{out}");
    assert_eq!(atlas::config::Config::load(&h.config()).unwrap().update.unwrap().pin, None);
    let (ok, out) = h.run(&["list"]);
    assert!(ok && out.contains("[update] Update atlas at session start  (off)"), "{out}");
    // Three configuration writes, three undos, and the template is back byte for byte.
    for _ in 0..3 {
        let (ok, out) = h.run(&["undo"]);
        assert!(ok, "{out}");
    }
    assert_eq!(h.read(&h.config()), template);
    assert!(!h.settings().exists(), "Claude Code's settings were never touched");
}

/// Every file under `dir`, relative, sorted.
fn files_under(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                out.push(e.path().strip_prefix(dir).unwrap().display().to_string());
            }
        }
    }
    out.sort();
    out
}

#[test]
fn an_atlas_config_edit_writes_nothing_under_the_claude_dir() {
    let h = Home::new("cs-atlas-backups");
    let (ok, out) = h.run(&["list"]);
    assert!(ok, "{out}");
    let template = h.read(&h.config());
    let (ok, out) = h.run(&["set", "update-pin", "0.1.0"]);
    assert!(ok, "{out}");
    assert_eq!(files_under(&h.claude()), Vec::<String>::new(), "nothing of atlas's own goes into Claude Code's folder");
    let beside = h.atlas().join("atlas-backups");
    let saved = files_under(&beside);
    assert!(saved.len() == 1 && saved[0].starts_with("atlas.") && saved[0].ends_with(".json"), "{saved:?}");
    assert!(out.contains(&beside.display().to_string()), "the backup's place is printed: {out}");
    // A settings.json write in between: undo still walks back newest first,
    // each kind from its own folder.
    let (ok, out) = h.run(&["set", "goal-checkin-120", "60"]);
    assert!(ok, "{out}");
    assert_eq!(files_under(&h.claude().join("atlas-backups")).len(), 1);
    assert_eq!(files_under(&beside).len(), 1, "a settings.json backup does not go beside the configuration");
    let (ok, out) = h.run(&["undo"]);
    assert!(ok && out.contains("did not exist"), "the settings.json write is undone first: {out}");
    assert!(!h.settings().exists());
    let (ok, out) = h.run(&["undo"]);
    assert!(ok, "{out}");
    assert_eq!(h.read(&h.config()), template);
    assert!(files_under(&beside).is_empty() && files_under(&h.claude()).is_empty());
    let (ok, out) = h.run(&["undo"]);
    assert!(!ok && out.contains("no atlas backup"), "{out}");
}

#[test]
fn the_interactive_offer_asks_skips_old_versions_and_writes_the_yeses() {
    let h = Home::new("cs-ask");
    std::fs::create_dir_all(h.atlas()).unwrap();
    let p = h.paths();
    // Claude Code too old for the cache setting: skipped with a note; the
    // check-in setting is asked and answered yes.
    let (ok, out) = scripted(&p, &["offer"], "y\n", Some((2, 1, 0)));
    assert!(ok, "{out}");
    assert!(out.contains("[subagent-cache-1h] skipped: needs Claude Code 2.1.242 or newer"), "{out}");
    assert!(out.contains("Turn this on? [y/N]") && out.contains("applied: goal-checkin-120"), "{out}");
    assert!(out.contains("[routing]") && out.contains("Turn off: atlas settings off routing"), "atlas's defaults are explained: {out}");
    let v = json::parse(&h.read(&h.settings())).unwrap();
    assert_eq!(v["env"]["CLAUDE_CODE_GOAL_CHECKIN_MINUTES"], "120");
    assert!(v.get("subagentPromptCacheTtl").is_none());
    // A new enough version: asked; "no" writes nothing.
    let before = h.read(&h.settings());
    let (ok, out) = scripted(&p, &["offer"], "n\n", Some((2, 1, 316)));
    assert!(ok && out.contains("[goal-checkin-120] already applied") && out.contains("nothing to change"), "{out}");
    assert_eq!(h.read(&h.settings()), before);
    // Without a terminal the offer only describes.
    let mut o = Vec::new();
    let mut i: &[u8] = b"y\n";
    cs::run(&["offer".to_string()], &p, &mut Io { out: &mut o, input: &mut i, interactive: false, version: None }).unwrap();
    let o = String::from_utf8(o).unwrap();
    assert!(o.contains("nothing written"), "{o}");
    assert_eq!(h.read(&h.settings()), before);
}

#[test]
fn the_session_start_line_is_offered_once_and_is_short() {
    let h = Home::new("cs-line");
    let p = h.paths();
    let line = cs::session_start_line(&p).unwrap();
    assert!(line.contains("2 Claude Code settings are offered (1h subagent cache, goal check-ins)"), "{line}");
    assert!(line.contains("On by default: routing, update"), "{line}");
    // At most 60 tokens at the ledger's 4 bytes a token.
    assert!(line.len() <= 240, "{} bytes: {line}", line.len());
    assert_eq!(cs::session_start_line(&p), None, "never repeated");
    // An applied setting is not offered; only a new manifest id would be.
    let h2 = Home::new("cs-line2");
    std::fs::write(h2.settings(), "{\n  \"subagentPromptCacheTtl\": \"1h\"\n}\n").unwrap();
    let line = cs::session_start_line(&h2.paths()).unwrap();
    assert!(line.contains("1 Claude Code setting is offered (goal check-ins)"), "{line}");
    // No place to record the state: nothing, rather than a line every session.
    let mut none = h2.paths();
    none.state = None;
    assert_eq!(cs::session_start_line(&none), None);
}

#[test]
fn the_session_start_hook_carries_the_line_once() {
    use atlas::hooks::{HookEvent, Hooks};
    let h = Home::new("cs-hook");
    let s = atlas::memory::settings::Settings::from_config(None, &h.atlas().join("state").join("index.sqlite")).unwrap();
    let mut hooks = Hooks::open(s).unwrap();
    hooks.set_offer_paths(Some(h.paths()));
    let ev = HookEvent { event: "SessionStart".into(), session_id: "s".into(), source: "startup".into(), ..HookEvent::default() };
    let a = hooks.handle(&ev);
    let j = json::parse(&a.to_json("SessionStart")).unwrap();
    assert!(j["hookSpecificOutput"]["additionalContext"].as_str().unwrap().starts_with("atlas: 2 Claude Code settings"));
    assert_eq!(hooks.handle(&ev).to_json("SessionStart"), "");
}

/// `atlas --hook` on a SessionStart event in `cwd`, as Claude Code runs it
/// (a command hook, no `--config`): the additional context it answers.
fn session_start(h: &Home, cwd: &Path) -> String {
    use std::io::Write;
    let mut child = Command::new(bin())
        .arg("--hook")
        .env("ATLAS_HOME", h.atlas())
        .env("CLAUDE_CONFIG_DIR", h.claude())
        .env("HOME", h.t.path())
        .env("USERPROFILE", h.t.path())
        .env_remove("ATLAS_CONFIG")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let ev = json!({"hook_event_name": "SessionStart", "session_id": "s", "source": "startup", "cwd": cwd.to_string_lossy().to_string()});
    child.stdin.take().unwrap().write_all(ev.to_string().as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.trim().is_empty() {
        return String::new();
    }
    let v = atlas::json::parse(&text).unwrap();
    v["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("").to_string()
}

#[test]
fn the_first_session_in_a_project_offers_it_once_and_add_project_indexes_it() {
    let h = Home::new("cs-project");
    let proj = h.t.path().join("work").join("Demo App");
    write(&proj.join("README.md"), "# Demo\n\nThe demo app.\n");
    write(&proj.join("docs/guide.md"), "# Guide\n\nHow to run it.\n");
    write(&proj.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n");
    write(&proj.join("src/lib.rs"), "pub fn demo() {}\n");
    write(&proj.join("target/debug/junk.md"), "# build output\n");
    // The first session there: one line, asking; never again for that folder.
    let first = session_start(&h, &proj);
    assert!(first.contains("this project (Demo App) is not indexed") && first.contains("atlas settings add-project"), "{first}");
    assert!(!session_start(&h, &proj).contains("this project"), "offered once");
    // A home folder is never offered.
    assert!(!session_start(&h, h.t.path()).contains("this project"));
    // Yes: the source and the code root are added, and the source is indexed.
    let (ok, text) = h.run(&["add-project", &proj.to_string_lossy()]);
    assert!(ok, "{text}");
    assert!(text.contains("indexed source demo-app: 2 Markdown files"), "{text}");
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert!(c.vaults.iter().any(|v| v.name == "notes"), "the notes vault stays");
    let src = c.sources.iter().find(|s| s.name == "demo-app").expect("source");
    assert!(src.include == ["**/*.md"] && src.exclude.iter().any(|e| e == "target/**"));
    assert_eq!(c.code.as_ref().unwrap().roots.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["demo-app-code"]);
    let db = h.atlas().join("state").join("atlas.sqlite");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM docs WHERE source = 'demo-app'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
    // Adding it again changes nothing; a folder inside it is not offered.
    let (ok, text) = h.run(&["add-project", &proj.to_string_lossy()]);
    assert!(ok && text.contains("already indexed"), "{text}");
    assert!(!session_start(&h, &proj.join("docs")).contains("this project"));
}