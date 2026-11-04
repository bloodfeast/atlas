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
        // Backups only: each change also leaves a `.label` beside its backup.
        std::fs::read_dir(self.claude().join("atlas-backups")).map(|r| r.flatten().filter(|e| !e.file_name().to_string_lossy().ends_with(".label")).count()).unwrap_or(0)
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
        Paths { claude_dir: self.claude(), atlas_config: Some(self.config()), state: Some(self.atlas().join("state").join(cs::OFFER_STATE)), db: Some(self.atlas().join("state").join("atlas.sqlite")), user_state: None, project: None }
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
    let code = cs::run(&args, p, &mut Io { out: &mut out, input: &mut input, interactive: true, version, dry_run: false }).unwrap();
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
    let (ok, out) = h.run(&["undo", "--yes"]);
    assert!(ok && out.contains("did not exist"), "{out}");
    assert!(!h.settings().exists());
    let (ok, out) = h.run(&["undo", "--yes"]);
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
        let (ok, out) = h.run(&["undo", "--yes"]);
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
        let (ok, out) = h.run(&["undo", "--yes"]);
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
    let saved: Vec<String> = files_under(&beside).into_iter().filter(|f| !f.ends_with(".label")).collect();
    assert!(saved.len() == 1 && saved[0].starts_with("atlas.") && saved[0].ends_with(".json"), "{saved:?}");
    assert!(out.contains(&beside.display().to_string()), "the backup's place is printed: {out}");
    // A settings.json write in between: undo still walks back newest first,
    // each kind from its own folder.
    let (ok, out) = h.run(&["set", "goal-checkin-120", "60"]);
    assert!(ok, "{out}");
    assert_eq!(h.backups(), 1);
    assert_eq!(files_under(&beside).iter().filter(|f| !f.ends_with(".label")).count(), 1, "a settings.json backup does not go beside the configuration");
    let (ok, out) = h.run(&["undo", "--yes"]);
    assert!(ok && out.contains("did not exist"), "the settings.json write is undone first: {out}");
    assert!(!h.settings().exists());
    let (ok, out) = h.run(&["undo", "--yes"]);
    assert!(ok, "{out}");
    assert_eq!(h.read(&h.config()), template);
    assert!(files_under(&beside).is_empty() && files_under(&h.claude()).is_empty());
    let (ok, out) = h.run(&["undo", "--yes"]);
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
    cs::run(&["offer".to_string()], &p, &mut Io { out: &mut o, input: &mut i, interactive: false, version: None, dry_run: false }).unwrap();
    let o = String::from_utf8(o).unwrap();
    assert!(o.contains("Nothing went wrong") && o.contains("no terminal here to answer in"), "{o}");
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

#[test]
fn add_project_makes_a_go_module_a_code_root() {
    let h = Home::new("cs-project-go");
    let proj = h.t.path().join("work").join("payments");
    write(&proj.join("README.md"), "# Payments\n\nThe service.\n");
    write(&proj.join("go.mod"), "module example.com/payments\n\ngo 1.22\n");
    let (ok, text) = h.run(&["add-project", &proj.to_string_lossy()]);
    assert!(ok, "{text}");
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert_eq!(c.code.as_ref().unwrap().roots.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["payments-code"], "a go.mod alone makes a code root");
    // Go files only in a vendored folder do not.
    let other = h.t.path().join("work").join("docs-site");
    write(&other.join("README.md"), "# Docs\n");
    write(&other.join("vendor/x/dep.go"), "package x\n");
    let (ok, text) = h.run(&["add-project", &other.to_string_lossy()]);
    assert!(ok, "{text}");
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert_eq!(c.code.as_ref().unwrap().roots.len(), 1, "{text}");
}

#[test]
fn add_project_makes_a_typescript_or_javascript_package_a_code_root() {
    let h = Home::new("cs-project-ts");
    let web = h.t.path().join("work").join("web");
    write(&web.join("README.md"), "# Web\n\nThe front end.\n");
    write(&web.join("tsconfig.json"), "{}\n");
    let cart = h.t.path().join("work").join("cart");
    write(&cart.join("package.json"), "{\"name\": \"cart\"}\n");
    for p in [&web, &cart] {
        let (ok, text) = h.run(&["add-project", &p.to_string_lossy()]);
        assert!(ok, "{text}");
    }
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert_eq!(
        c.code.as_ref().unwrap().roots.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["web-code", "cart-code"],
        "a tsconfig.json or a package.json alone makes a code root"
    );
    // Scripts only in a dependency or build folder do not.
    let other = h.t.path().join("work").join("notes");
    write(&other.join("README.md"), "# Notes\n");
    write(&other.join("node_modules/x/index.js"), "module.exports = 1;\n");
    write(&other.join("dist/app.js"), "var a = 1;\n");
    let (ok, text) = h.run(&["add-project", &other.to_string_lossy()]);
    assert!(ok, "{text}");
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert_eq!(c.code.as_ref().unwrap().roots.len(), 2, "{text}");
}

/// `atlas --rescan` in the scratch home: the configuration loads and every
/// vault opens (a vault whose folder is missing, or two vault folders with
/// one name, would fail here as they would stop the server).
fn rescan(h: &Home) -> (bool, String) {
    let out = Command::new(bin())
        .arg("--rescan")
        .env("ATLAS_HOME", h.atlas())
        .env("CLAUDE_CONFIG_DIR", h.claude())
        .env("HOME", h.t.path())
        .env("USERPROFILE", h.t.path())
        .env_remove("ATLAS_CONFIG")
        .output()
        .unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// A folder as atlas shows it: `/`-separated.
fn shown(p: &Path) -> String {
    atlas::paths::canonical(p).to_string_lossy().replace('\\', "/")
}

#[test]
fn the_first_session_offers_a_vault_yes_creates_it_and_undo_removes_it() {
    let h = Home::new("cs-vault");
    let proj = h.t.path().join("work").join("Demo App");
    write(&proj.join("README.md"), "# Demo\n\nThe demo app.\n");
    write(&proj.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n");
    write(&proj.join("src/lib.rs"), "pub fn demo() {}\n");
    // The first session: one line, the vault folded in, default yes.
    let first = session_start(&h, &proj);
    let at = shown(&h.atlas().join("vaults").join("demo-app-notes"));
    assert!(first.contains(&format!("create a vault for this project at {at}? yes / other folder / no")), "{first}");
    assert!(first.contains("yes is the default") && first.contains("atlas settings add-project --vault \"") && first.contains("--vault-at \"FOLDER\""), "{first}");
    assert!(first.contains("on no nothing (atlas will not ask again)") && first.contains("atlas settings undo"), "{first}");
    // Asked once: the second session says nothing about it.
    assert!(!session_start(&h, &proj).contains("this project"), "asked once");
    // Yes: the source, the code root and the vault, in one write.
    let (ok, text) = h.run(&["add-project", "--vault", &proj.to_string_lossy()]);
    assert!(ok, "{text}");
    assert!(text.contains("added source demo-app, code root demo-app-code, vault demo-app-notes"), "{text}");
    assert!(text.contains(&format!("vault demo-app-notes: {at} (created, with README.md)")), "{text}");
    let vault = h.atlas().join("vaults").join("demo-app-notes");
    assert!(h.read(&vault.join("README.md")).contains("Open folder as vault"));
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert_eq!(c.vaults.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["notes", "demo-app-notes"]);
    assert!(c.sources.iter().any(|s| s.name == "demo-app") && c.code.as_ref().is_some_and(|k| k.roots.len() == 1));
    let (ok, out) = rescan(&h);
    assert!(ok, "the server opens every vault: {out}");
    // Asking again adds nothing.
    let (ok, text) = h.run(&["add-project", "--vault", &proj.to_string_lossy()]);
    assert!(ok && text.contains("has its vault already") && !text.contains("added"), "{text}");
    // One undo reverts the source, the code root and the vault; the folder
    // atlas made, holding only its note, goes with it.
    let (ok, text) = h.run(&["undo", "--yes"]);
    assert!(ok, "{text}");
    assert!(text.contains("its folder removed (atlas created it and it held no notes of yours)"), "{text}");
    assert!(!vault.exists());
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert!(c.vaults.len() == 1 && c.sources.is_empty() && c.code.is_none(), "one undo reverts the whole answer");
    // Still asked once after the undo.
    assert!(!session_start(&h, &proj).contains("this project"));
    // Yes again, a note written into the vault, then undo: the notes stay.
    let (ok, text) = h.run(&["add-project", "--vault", &proj.to_string_lossy()]);
    assert!(ok, "{text}");
    write(&vault.join("Plan.md"), "# Plan\n\nMine.\n");
    let (ok, text) = h.run(&["undo", "--yes"]);
    assert!(ok && text.contains("the folder is kept because it holds notes"), "{text}");
    assert!(vault.join("Plan.md").is_file() && vault.join("README.md").is_file());
    assert_eq!(atlas::config::Config::load(&h.config()).unwrap().vaults.len(), 1);
    // The kept folder is reused by the next yes, as it is.
    let (ok, text) = h.run(&["add-project", "--vault", &proj.to_string_lossy()]);
    assert!(ok && text.contains("(an existing folder, left as it is)"), "{text}");
    let (ok, text) = h.run(&["undo", "--yes"]);
    assert!(ok && text.contains("the folder was there before atlas registered it and is kept"), "{text}");
    assert!(vault.join("Plan.md").is_file());
}

#[test]
fn another_folder_for_the_vault_is_checked_and_kept_out_of_the_source() {
    let h = Home::new("cs-vault-at");
    let proj = h.t.path().join("work").join("shop");
    write(&proj.join("README.md"), "# Shop\n");
    // A folder named like another vault's (the first run's `notes`) is
    // refused, before anything is written.
    let (ok, text) = h.run(&["add-project", "--vault-at", &proj.join("notes").to_string_lossy(), &proj.to_string_lossy()]);
    assert!(!ok && text.contains("also named \"notes\""), "{text}");
    assert!(!proj.join("notes").exists());
    assert!(atlas::config::Config::load(&h.config()).unwrap().sources.is_empty());
    // The project folder itself is refused.
    let (ok, text) = h.run(&["add-project", "--vault-at", &proj.to_string_lossy(), &proj.to_string_lossy()]);
    assert!(!ok && text.contains("is the project folder or holds it"), "{text}");
    // A folder inside the project: the source leaves it to the vault.
    let docs = proj.join("docs").join("vault");
    write(&docs.join("Idea.md"), "# Idea\n");
    let (ok, text) = h.run(&["add-project", "--vault-at", &docs.to_string_lossy(), &proj.to_string_lossy()]);
    assert!(ok, "{text}");
    assert!(text.contains("(an existing folder, left as it is)") && !docs.join("README.md").exists(), "{text}");
    let c = atlas::config::Config::load(&h.config()).unwrap();
    let src = c.sources.iter().find(|s| s.name == "shop").unwrap();
    assert!(src.exclude.iter().any(|e| e == "docs/vault/**"), "{:?}", src.exclude);
    assert!(text.contains("indexed source shop: 1 Markdown file"), "the vault's note is not the source's: {text}");
    let v = c.vaults.iter().find(|v| v.name == "shop-notes").unwrap();
    assert_eq!(shown(&v.path), shown(&docs));
    let (ok, out) = rescan(&h);
    assert!(ok, "{out}");
    // Undo unregisters and keeps the folder that was there.
    let (ok, text) = h.run(&["undo", "--yes"]);
    assert!(ok && text.contains("is kept"), "{text}");
    assert!(docs.join("Idea.md").is_file());
    // --vault does not go with --to, nor --no-vault without --new.
    let (ok, text) = h.run(&["add-project", "--vault", "--to", "x", &proj.to_string_lossy()]);
    assert!(!ok && text.contains("not with --to"), "{text}");
    let (ok, text) = h.run(&["add-project", "--no-vault", &proj.to_string_lossy()]);
    assert!(!ok && text.contains("--no-vault goes with --new"), "{text}");
}

#[test]
fn an_indexed_project_gets_its_vault_alone() {
    let h = Home::new("cs-vault-indexed");
    let proj = h.t.path().join("work").join("api");
    write(&proj.join("README.md"), "# API\n");
    let (ok, text) = h.run(&["add-project", &proj.to_string_lossy()]);
    assert!(ok, "{text}");
    let (ok, text) = h.run(&["add-project", "--vault", &proj.to_string_lossy()]);
    assert!(ok && text.contains("added vault api-notes") && !text.contains("added source"), "{text}");
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert_eq!(c.sources.len(), 1);
    assert!(c.vaults.iter().any(|v| v.name == "api-notes"));
    // Undo takes the vault alone; the source stays.
    let (ok, text) = h.run(&["undo", "--yes"]);
    assert!(ok, "{text}");
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert!(c.sources.len() == 1 && c.vaults.len() == 1);
}

#[test]
fn compact_md_is_asked_once_written_on_yes_and_undone() {
    let h = Home::new("cs-compact-md");
    let p = h.paths();
    let proj = h.t.path().join("work").join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    let md = proj.join("CLAUDE.md");
    std::fs::write(&md, "# Project\n\nKeep tests green.\n").unwrap();
    let dir = proj.to_string_lossy().to_string();
    // Compaction is experimental and off by default: nothing is offered,
    // asked or written, and the offer is not spent.
    let (ok, out) = scripted(&p, &["compact-md", &dir, "--yes"], "", None);
    assert!(!ok && out.contains("compact") && out.contains("enabled") && out.contains("Nothing was changed"), "{out}");
    assert_eq!(h.read(&md), "# Project\n\nKeep tests green.\n");
    assert!(!cs::compaction_on(&p));
    std::fs::create_dir_all(h.atlas()).unwrap();
    std::fs::write(h.config(), r#"{"vaults": [], "compact": {"enabled": true}}"#).unwrap();
    assert!(cs::compaction_on(&p));
    // No: nothing written.
    let (ok, out) = scripted(&p, &["compact-md", &dir], "n\n", None);
    assert!(ok && out.contains("## Compact instructions") && out.contains("nothing written"), "{out}");
    assert_eq!(h.read(&md), "# Project\n\nKeep tests green.\n");
    // Asked once: the session line is not offered again for this folder.
    assert!(cs::compact_md_line(&p, &proj).is_none());
    // Yes: appended after a blank line, with a backup; idempotent.
    let (ok, out) = scripted(&p, &["compact-md", &dir, "--yes"], "", None);
    assert!(ok && out.contains("added to"), "{out}");
    let text = h.read(&md);
    assert!(text.starts_with("# Project\n\nKeep tests green.\n\n<!-- atlas:compact-instructions") && text.ends_with("<!-- /atlas:compact-instructions -->\n"), "{text}");
    let (_, out) = scripted(&p, &["compact-md", &dir, "--yes"], "", None);
    assert!(out.contains("already in"), "{out}");
    // An edit made since is kept: undo removes only the block.
    std::fs::write(&md, text.replace("Keep tests green.", "Keep tests green and fast.")).unwrap();
    let (ok, out) = scripted(&p, &["undo", "--yes"], "", None);
    assert!(ok && out.contains("removed the Compact instructions"), "{out}");
    assert_eq!(h.read(&md), "# Project\n\nKeep tests green and fast.\n");
    // A CLAUDE.md atlas created is removed by undo.
    let fresh = h.t.path().join("work").join("fresh");
    std::fs::create_dir_all(fresh.join(".git")).unwrap();
    assert!(cs::compact_md_line(&p, &fresh).is_some_and(|l| l.contains("atlas settings compact-md --yes")));
    assert!(cs::compact_md_line(&p, &fresh).is_none(), "offered once");
    let (ok, _) = scripted(&p, &["compact-md", &fresh.to_string_lossy(), "--yes"], "", None);
    assert!(ok && fresh.join("CLAUDE.md").exists());
    let (ok, out) = scripted(&p, &["undo", "--yes"], "", None);
    assert!(ok && out.contains("atlas created it"), "{out}");
    assert!(!fresh.join("CLAUDE.md").exists());
    // Never offered outside a project folder.
    assert!(cs::compact_md_line(&p, &h.t.path().join("work")).is_none());
}

/// Without a terminal (and without --yes) the offer asks nothing, so it
/// records nothing as offered: the next run in a terminal still asks.
#[test]
fn an_offer_without_a_terminal_marks_nothing_offered() {
    let h = Home::new("cs-noterm");
    let p = h.paths();
    let mut o = Vec::new();
    let mut i: &[u8] = b"";
    cs::run(&["offer".to_string()], &p, &mut Io { out: &mut o, input: &mut i, interactive: false, version: None, dry_run: false }).unwrap();
    let state = p.state.as_ref().and_then(|s| std::fs::read_to_string(s).ok()).unwrap_or_default();
    let offered = json::parse(&state).ok().and_then(|v| v.get("offered").and_then(|a| a.as_array().map(Vec::len))).unwrap_or(0);
    assert_eq!(offered, 0, "{state}");
    let (ok, out) = scripted(&p, &["offer"], "n\nn\n", Some((2, 1, 316)));
    assert!(ok && out.contains("Turn this on?"), "a terminal run still asks: {out}");
}

/// `settings undo` shows what it would revert (when, which change, what
/// each file goes back to) and asks; without a terminal it needs --yes. A
/// command that changed nothing makes no backup, so the newest backup can
/// be an older, unrelated change: it is never restored unseen.
#[test]
fn undo_shows_the_change_and_asks_before_reverting() {
    let h = Home::new("cs-undo-confirm");
    let (ok, out) = h.run(&["offer", "--yes", "subagent-cache-1h"]);
    assert!(ok, "{out}");
    // A no-op: nothing to change, no backup.
    let (ok, out) = h.run(&["offer", "--yes", "subagent-cache-1h"]);
    assert!(ok && out.contains("nothing to change"), "{out}");
    let before = h.read(&h.settings());
    // Without a terminal: shown, refused, nothing reverted.
    let (ok, out) = h.run(&["undo"]);
    assert!(!ok && out.contains("reverts the newest atlas change, made 20") && out.contains(" UTC"), "{out}");
    assert!(out.contains("subagent-cache-1h") && out.contains("remove ") && out.contains("settings.json"), "the change is named: {out}");
    assert!(out.contains("atlas settings undo --yes"), "{out}");
    assert_eq!(h.read(&h.settings()), before);
    // In a terminal: "no" keeps it, "yes" reverts it.
    let p = h.paths();
    let (ok, out) = scripted(&p, &["undo"], "n\n", None);
    assert!(ok && out.contains("Revert it? [y/N]") && out.contains("nothing reverted"), "{out}");
    assert_eq!(h.read(&h.settings()), before);
    let (ok, out) = scripted(&p, &["undo"], "y\n", None);
    assert!(ok && out.contains("did not exist"), "{out}");
    assert!(!h.settings().exists());
}

/// A build from source never updates itself, so the offer does not list the
/// update setting as simply "on".
#[test]
fn a_source_build_does_not_call_updates_on() {
    let h = Home::new("cs-srcbuild");
    let p = h.paths();
    let mut o = Vec::new();
    let mut i: &[u8] = b"";
    cs::run(&["offer".to_string()], &p, &mut Io { out: &mut o, input: &mut i, interactive: false, version: None, dry_run: false }).unwrap();
    let o = String::from_utf8(o).unwrap();
    let line = o.lines().find(|l| l.starts_with("[update]")).unwrap_or_else(|| panic!("{o}"));
    assert!(atlas::update::release_build() || line.contains("off here: a build from source never updates itself"), "{line}");
}
