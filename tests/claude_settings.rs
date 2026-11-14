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

/// Whether the library entry answers with an error.
fn refused(p: &Paths, args: &[&str]) -> bool {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut input) = (Vec::new(), &b""[..]);
    cs::run(&args, p, &mut Io { out: &mut out, input: &mut input, interactive: false, version: None, dry_run: false, machine: None }).is_err()
}

/// The machine scripted answers run on: 32 hardware threads, Claude Code's
/// default subagent limit (compile 2, caps 2, 2, 1).
const MACHINE: cs::Machine = cs::Machine { cores: 32, claude_limit: 20 };

/// The library entry with scripted answers and a given Claude Code version.
fn scripted(p: &Paths, args: &[&str], answers: &str, version: Option<(u32, u32, u32)>) -> (bool, String) {
    let mut out = Vec::new();
    let mut input = answers.as_bytes();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let code = cs::run(&args, p, &mut Io { out: &mut out, input: &mut input, interactive: true, version, dry_run: false, machine: Some(MACHINE) }).unwrap();
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
    // The settings.json write left its lock file, which is never removed.
    assert!(files_under(&beside).is_empty() && files_under(&h.claude()) == ["settings.json.lock"], "{:?}", files_under(&h.claude()));
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
    cs::run(&["offer".to_string()], &p, &mut Io { out: &mut o, input: &mut i, interactive: false, version: None, dry_run: false, machine: None }).unwrap();
    let o = String::from_utf8(o).unwrap();
    assert!(o.contains("Nothing went wrong") && o.contains("no terminal here to answer in"), "{o}");
    assert_eq!(h.read(&h.settings()), before);
}

#[test]
fn the_session_start_line_returns_until_answered_and_is_short() {
    let h = Home::new("cs-line");
    let p = h.paths();
    let line = cs::session_start_line(&p).unwrap();
    assert!(line.contains("2 Claude Code settings are offered (1h subagent cache, goal check-ins)"), "{line}");
    assert!(line.contains("On by default: routing, update, compact"), "{line}");
    // At most 60 tokens at the ledger's 4 bytes a token.
    assert!(line.len() <= 240, "{} bytes: {line}", line.len());
    // The question stays until answered; the notice was shown and is done.
    let next = cs::session_start_line(&p).unwrap();
    assert!(next.contains("2 Claude Code settings are offered") && next.contains("offer --no") && !next.contains("On by default"), "{next}");
    assert_eq!(cs::session_start_line(&p).as_ref(), Some(&next), "unanswered, it returns");
    let (ok, out) = scripted(&p, &["offer", "--no"], "", None);
    assert!(ok && !h.settings().exists(), "{out}");
    assert_eq!(cs::session_start_line(&p), None, "no is an answer");
    assert!(refused(&p, &["offer", "--no", "subagent-cache-1h"]), "--no takes nothing else");
    // An applied setting is not offered; only a new manifest id would be.
    let h2 = Home::new("cs-line2");
    std::fs::write(h2.settings(), "{\n  \"subagentPromptCacheTtl\": \"1h\"\n}\n").unwrap();
    let line = cs::session_start_line(&h2.paths()).unwrap();
    assert!(line.contains("1 Claude Code setting is offered (goal check-ins)"), "{line}");
    // No place to record the state: nothing, rather than a line every session.
    let mut none = h2.paths();
    none.state = None;
    assert_eq!(cs::session_start_line(&none), None);
    // A state that cannot be written: silence, not a question whose answer is
    // lost, even with the notice already shown (no write is tried for it).
    #[cfg(windows)]
    {
        let h3 = Home::new("cs-line3");
        let p = h3.paths();
        cs::session_start_line(&p).unwrap();
        let state = p.state.clone().unwrap();
        let mut perm = std::fs::metadata(&state).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&state, perm.clone()).unwrap();
        assert_eq!(cs::session_start_line(&p), None);
        assert!(refused(&p, &["offer", "--no"]), "the no could not be recorded");
        #[allow(clippy::permissions_set_readonly_false)] // Windows only; the temp folder must be removable.
        perm.set_readonly(false);
        std::fs::set_permissions(&state, perm).unwrap();
    }
}

/// A yes or a no to the settings is no answer to the orchestrated question: that
/// question has its own, and nobody asked it.
#[test]
fn answering_the_settings_leaves_the_orchestrated_question() {
    use atlas::hooks::{HookEvent, Hooks};
    for (tag, args) in [("cs-yes-orch", &["offer", "--yes", "subagent-cache-1h"][..]), ("cs-no-orch", &["offer", "--no"][..])] {
        let h = Home::new(tag);
        let p = h.paths();
        let (ok, out) = scripted(&p, args, "", None);
        assert!(ok, "{tag}: {out}");
        let s = atlas::memory::settings::Settings::from_config(None, &h.atlas().join("state").join("atlas.sqlite")).unwrap();
        let mut hooks = Hooks::open(s).unwrap();
        hooks.set_offer_paths(Some(p));
        let ev = HookEvent { event: "SessionStart".into(), session_id: "s".into(), source: "startup".into(), ..HookEvent::default() };
        let a = hooks.handle(&ev).to_json("SessionStart");
        assert!(a.contains("orchestrated mode is offered"), "{tag}: {a}");
        assert!(!a.contains("Claude Code setting") && !a.contains("On by default"), "{tag}: {a}");
    }
}

/// A settings.json that does not parse stops every yes, so it must not stop a no:
/// the questions would return each session with no way to answer them.
#[test]
fn a_no_is_recorded_whatever_settings_json_holds() {
    let h = Home::new("cs-bad-settings");
    let p = h.paths();
    let proj = h.t.path().join("work").join("demo");
    write(&proj.join("README.md"), "# Demo\n");
    std::fs::write(h.settings(), "{ not json").unwrap();
    assert!(refused(&p, &["offer", "--yes", "subagent-cache-1h"]), "a yes still needs the file");
    assert!(cs::session_start_line(&p).is_some());
    let dir = proj.to_string_lossy().into_owned();
    for args in [&["offer", "--no"][..], &["orchestrate", "--no"], &["add-project", "--no", dir.as_str()]] {
        assert!(!refused(&p, args), "{args:?}");
    }
    assert_eq!(cs::session_start_line(&p), None);
    assert_eq!(cs::orchestrate_line(&p), None);
    let state = std::fs::read_to_string(p.state.as_ref().unwrap()).unwrap();
    assert!(state.contains("\"projects\"") && state.contains("demo"), "{state}");
}

/// A `--yes` that turned nothing on is no answer: the setting is off, and the question
/// has to outlast the reason, or an upgrade finds it gone for good.
#[test]
fn a_yes_that_applied_nothing_leaves_the_question() {
    let asks = |p: &cs::Paths| cs::session_start_line(p).is_some_and(|l| l.contains("Claude Code setting"));
    let h = Home::new("cs-yes-nothing");
    let p = h.paths();
    let id = "subagent-cache-1h";
    // Too old a Claude Code, alone and beside one that lands: that one is written,
    // and the question and the failure stand.
    let (ok, out) = scripted(&p, &["offer", "--yes", id], "", Some((0, 0, 1)));
    assert!(!ok && out.contains("skipped") && out.contains("offer --no"), "{out}");
    assert!(asks(&p), "{out}");
    let mixed = Home::new("cs-yes-mixed");
    let mp = mixed.paths();
    let (ok, out) = scripted(&mp, &["offer", "--yes", id, "goal-checkin-120"], "", Some((0, 0, 1)));
    assert!(!ok && out.contains("skipped"), "{out}");
    assert!(std::fs::read_to_string(mixed.settings()).unwrap().contains("120"), "{out}");
    assert!(asks(&mp), "{out}");
    // An atlas switch beside one that lands: written, exit 0, unanswered.
    let mixed = Home::new("cs-yes-switch");
    let mp = mixed.paths();
    let (ok, out) = scripted(&mp, &["offer", "--yes", "routing", "goal-checkin-120"], "", None);
    assert!(ok, "{out}");
    assert!(std::fs::read_to_string(mixed.settings()).unwrap().contains("120"), "{out}");
    assert!(!std::fs::read_to_string(mp.state.as_ref().unwrap()).unwrap_or_default().contains("subagent-cache-1h"), "{out}");
    // An atlas switch changes nothing and answers nothing; naming none is refused.
    let (ok, out) = scripted(&p, &["offer", "--yes", "routing"], "", None);
    assert!(ok && out.contains("atlas switch"), "{out}");
    assert!(asks(&p), "{out}");
    assert!(refused(&p, &["offer", "--yes"]));
    assert!(asks(&p));
    // Named and applied: answered.
    let (ok, out) = scripted(&p, &["offer", "--yes", id], "", None);
    assert!(ok, "{out}");
    assert!(!asks(&p), "{out}");
    // Already on is a yes that holds: answered too.
    let h = Home::new("cs-yes-already");
    let p = h.paths();
    assert!(scripted(&p, &["offer", "--yes", id], "", None).0);
    let state = p.state.clone().unwrap();
    std::fs::remove_file(&state).unwrap();
    let (ok, out) = scripted(&p, &["offer", "--yes", id], "", None);
    assert!(ok && out.contains("already applied"), "{out}");
    assert!(std::fs::read_to_string(&state).unwrap_or_default().contains(id), "{out}");
}

/// At a terminal, a setting too new for this Claude Code was never put to the user,
/// so that one is still to ask; the rest they answered.
#[test]
fn a_terminal_does_not_answer_for_a_setting_it_did_not_ask() {
    let h = Home::new("cs-term-skip");
    let p = h.paths();
    let (ok, out) = scripted(&p, &["offer"], "n\nn\nn\nn\nn\nn\n", Some((0, 0, 1)));
    assert!(ok && out.contains("skipped"), "{out}");
    let state = std::fs::read_to_string(p.state.as_ref().unwrap()).unwrap();
    assert!(!state.contains("subagent-cache-1h"), "{state}");
    assert!(state.contains("orchestrate"), "{state}");
}

/// A terminal's yes that lands is an answer.
#[test]
fn a_terminal_yes_that_landed_is_marked() {
    let h = Home::new("cs-term-lands");
    let p = h.paths();
    let (ok, out) = scripted(&p, &["offer"], "y\nn\nn\nn\nn\nn\n", None);
    assert!(ok, "{out}");
    assert!(std::fs::read_to_string(h.settings()).unwrap().contains("CLAUDE_CODE") || out.contains("applied"), "{out}");
    let state = std::fs::read_to_string(p.state.as_ref().unwrap()).unwrap();
    assert!(state.contains("subagent-cache-1h"), "{state}");
}

/// A terminal's yes that could not be written is no answer either; its no is one.
#[cfg(windows)]
#[test]
fn a_terminal_yes_that_did_not_land_leaves_the_question() {
    let h = Home::new("cs-term-fails");
    let p = h.paths();
    std::fs::write(h.settings(), "{}\n").unwrap();
    let mut perm = std::fs::metadata(h.settings()).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(h.settings(), perm.clone()).unwrap();
    let mut out = Vec::new();
    let mut input = &b"y\nn\nn\nn\nn\nn\n"[..];
    let args = vec!["offer".to_string()];
    let r = cs::run(&args, &p, &mut cs::Io { out: &mut out, input: &mut input, interactive: true, version: None, dry_run: false, machine: None });
    assert!(r.is_err() || r.unwrap() != std::process::ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
    #[allow(clippy::permissions_set_readonly_false)] // Windows only; the temp folder must be removable.
    perm.set_readonly(false);
    std::fs::set_permissions(h.settings(), perm).unwrap();
    let state = std::fs::read_to_string(p.state.as_ref().unwrap()).unwrap_or_default();
    assert!(!state.contains("subagent-cache-1h"), "the yes: {state}");
    assert!(cs::session_start_line(&p).unwrap().contains("setting"), "{state}");
}

/// A yes that could not be written is no answer, so the question stays.
#[cfg(windows)]
#[test]
fn a_yes_that_did_not_land_leaves_the_question() {
    let h = Home::new("cs-yes-fails");
    let p = h.paths();
    std::fs::write(h.settings(), "{}\n").unwrap();
    let mut perm = std::fs::metadata(h.settings()).unwrap().permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(h.settings(), perm.clone()).unwrap();
    assert!(refused(&p, &["offer", "--yes", "subagent-cache-1h"]) || !scripted(&p, &["offer", "--yes", "subagent-cache-1h"], "", None).0);
    #[allow(clippy::permissions_set_readonly_false)] // Windows only; the temp folder must be removable.
    perm.set_readonly(false);
    std::fs::set_permissions(h.settings(), perm).unwrap();
    assert!(cs::session_start_line(&p).unwrap().contains("Claude Code settings are offered"));
}

/// `unmapped_line`'s notice has one showing: a session with no user, or a resume,
/// does not spend it.
#[test]
fn the_unmapped_notice_waits_for_a_new_attended_session() {
    use atlas::hooks::{HookEvent, Hooks};
    let h = Home::new("cs-unmapped");
    let s = atlas::memory::settings::Settings::from_config(None, &h.atlas().join("state").join("atlas.sqlite")).unwrap();
    let mut hooks = Hooks::open(s).unwrap();
    hooks.set_offer_paths(Some(h.paths()));
    hooks.set_offer_ctx(Some(cs::OfferCtx { scoped: true, projects: vec!["acme".into()], known: Vec::new(), project: "acme".into(), unmapped: vec!["reports".into()] }));
    let state = h.paths().state.unwrap();
    let start = HookEvent { event: "SessionStart".into(), session_id: "s".into(), source: "startup".into(), ..HookEvent::default() };
    let spent = || std::fs::read_to_string(&state).is_ok_and(|t| t.contains("acme:reports"));
    hooks.set_unattended(true);
    let a = hooks.handle(&start).to_json("SessionStart");
    assert!(!a.contains("no local checkout") && !spent(), "{a}");
    hooks.set_unattended(false);
    let a = hooks.handle(&HookEvent { source: "resume".into(), ..start.clone() }).to_json("SessionStart");
    assert!(!a.contains("no local checkout") && !spent(), "{a}");
    let a = hooks.handle(&start).to_json("SessionStart");
    assert!(a.contains("no local checkout") && spent(), "{a}");
    // A notice: shown is done, so a misread headless session would spend it too.
    assert!(!hooks.handle(&start).to_json("SessionStart").contains("no local checkout"));
}

#[test]
fn the_session_start_hook_carries_the_line_until_answered() {
    use atlas::hooks::{HookEvent, Hooks};
    let h = Home::new("cs-hook");
    let s = atlas::memory::settings::Settings::from_config(None, &h.atlas().join("state").join("index.sqlite")).unwrap();
    let mut hooks = Hooks::open(s).unwrap();
    hooks.set_offer_paths(Some(h.paths()));
    let ev = HookEvent { event: "SessionStart".into(), session_id: "s".into(), source: "startup".into(), ..HookEvent::default() };
    let a = hooks.handle(&ev);
    let j = json::parse(&a.to_json("SessionStart")).unwrap();
    assert!(j["hookSpecificOutput"]["additionalContext"].as_str().unwrap().starts_with("atlas: 2 Claude Code settings"));
    // Both questions return until answered; the notice was shown and is done.
    let again = hooks.handle(&ev).to_json("SessionStart");
    assert!(again.contains("Claude Code settings") && again.contains("orchestrated mode is offered") && !again.contains("On by default"), "{again}");
    // A resume is the same session: no question is put again.
    let resume = HookEvent { source: "resume".into(), ..ev.clone() };
    assert_eq!(hooks.handle(&resume).to_json("SessionStart"), "");
}

/// A folder above home is deep enough to pass the component count, and is too
/// wide all the same: it would hold home and all under it.
#[test]
fn a_folder_above_home_is_not_offered() {
    use std::io::Write;
    let h = Home::new("cs-above-home");
    let (ok, _) = h.run(&["list"]);
    assert!(ok);
    let above = h.t.path().join("users");
    let home = above.join("me");
    let beside = above.join("other");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&beside).unwrap();
    let ask = |cwd: &Path| {
        let mut child = Command::new(bin())
            .arg("--hook")
            .env_remove("CLAUDE_CODE_SESSION_ATTENDED")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            .env("ATLAS_HOME", h.atlas())
            .env("CLAUDE_CONFIG_DIR", h.claude())
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env_remove("ATLAS_CONFIG")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let ev = json!({"hook_event_name": "SessionStart", "session_id": "s", "source": "startup", "cwd": cwd.to_string_lossy().to_string()});
        child.stdin.take().unwrap().write_all(ev.to_string().as_bytes()).unwrap();
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).to_string()
    };
    assert!(ask(&beside).contains("is not indexed"), "the fixture does offer a plain folder");
    assert!(!ask(&above).contains("is not indexed"));
}

/// `atlas --hook` on a SessionStart event in `cwd`, as Claude Code runs it
/// (a command hook, no `--config`): the additional context it answers.
fn session_start(h: &Home, cwd: &Path) -> String {
    session_start_in(h, cwd, &[])
}

/// [`session_start`] with the session's kind in the environment, as Claude Code
/// sets it for a hook; the test run's own is never inherited.
fn session_start_in(h: &Home, cwd: &Path, session: &[(&str, &str)]) -> String {
    use std::io::Write;
    let mut child = Command::new(bin())
        .arg("--hook")
        .env_remove("CLAUDE_CODE_SESSION_ATTENDED")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .envs(session.iter().copied())
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
    // The first session there: one line, asking, until it is answered.
    let first = session_start(&h, &proj);
    assert!(first.contains("this project (Demo App) is not indexed") && first.contains("atlas settings add-project"), "{first}");
    assert!(session_start(&h, &proj).contains("this project"), "unanswered, so it returns");
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
    assert!(first.contains("atlas settings add-project --no \"") && first.contains("atlas settings undo"), "{first}");
    // Unanswered, so the second session asks again.
    assert!(session_start(&h, &proj).contains("this project"), "it returns");
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

/// The block is a manual choice: with compaction on, a session in a project
/// folder is not offered it, and the command still writes it.
#[test]
fn session_start_does_not_offer_the_compact_instructions() {
    use atlas::hooks::{HookEvent, Hooks};
    let h = Home::new("cs-compact-md-no-offer");
    let p = h.paths();
    let proj = h.t.path().join("work").join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    let md = proj.join("CLAUDE.md");
    std::fs::write(&md, "# Project\n").unwrap();
    std::fs::create_dir_all(h.atlas()).unwrap();
    std::fs::write(h.config(), r#"{"vaults": [], "compact": {"enabled": true}}"#).unwrap();
    let mut s = atlas::memory::settings::Settings::from_config(None, &h.atlas().join("state").join("index.sqlite")).unwrap();
    s.compact.enabled = true;
    let mut hooks = Hooks::open(s).unwrap();
    hooks.set_offer_paths(Some(h.paths()));
    let dir = proj.to_string_lossy().to_string();
    let ctx = hooks.handle(&HookEvent { event: "SessionStart".into(), session_id: "s1".into(), source: "startup".into(), cwd: dir.clone(), ..HookEvent::default() }).context;
    assert!(!ctx.contains("compact-md") && !ctx.contains("Compact instructions"), "{ctx}");
    assert_eq!(h.read(&md), "# Project\n");
    let (ok, out) = scripted(&p, &["compact-md", &dir, "--yes"], "", None);
    assert!(ok && out.contains("added to"), "{out}");
    assert!(h.read(&md).contains("## Compact instructions"));
}

#[test]
fn compact_md_is_written_on_yes_and_undone() {
    let h = Home::new("cs-compact-md");
    let p = h.paths();
    let proj = h.t.path().join("work").join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    let md = proj.join("CLAUDE.md");
    std::fs::write(&md, "# Project\n\nKeep tests green.\n").unwrap();
    let dir = proj.to_string_lossy().to_string();
    // Compaction is on by default: no configuration, no `compact` section, or
    // one without `enabled`. Only an explicit false turns it off, and then
    // nothing is asked or written.
    assert!(cs::compaction_on(&p), "no configuration");
    std::fs::create_dir_all(h.atlas()).unwrap();
    for text in [r#"{"vaults": []}"#, r#"{"vaults": [], "compact": {"card_tokens": 400}}"#, r#"{"vaults": [], "compact": {"enabled": true}}"#] {
        std::fs::write(h.config(), text).unwrap();
        assert!(cs::compaction_on(&p), "{text}");
    }
    // `atlas settings off compact` writes the false (removing the key would
    // leave it on).
    let (ok, out) = scripted(&p, &["off", "compact"], "", None);
    assert!(ok && out.contains("[compact] turned off"), "{out}");
    assert!(h.read(&h.config()).contains("\"enabled\": false"), "{}", h.read(&h.config()));
    assert!(!cs::compaction_on(&p));
    let (ok, out) = scripted(&p, &["compact-md", &dir, "--yes"], "", None);
    assert!(!ok && out.contains("compact") && out.contains("enabled") && out.contains("Nothing was changed"), "{out}");
    assert_eq!(h.read(&md), "# Project\n\nKeep tests green.\n");
    std::fs::write(h.config(), r#"{"vaults": [], "compact": {"enabled": true}}"#).unwrap();
    assert!(cs::compaction_on(&p));
    // No: nothing written.
    let (ok, out) = scripted(&p, &["compact-md", &dir], "n\n", None);
    assert!(ok && out.contains("## Compact instructions") && out.contains("nothing written"), "{out}");
    assert_eq!(h.read(&md), "# Project\n\nKeep tests green.\n");
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
    let (ok, _) = scripted(&p, &["compact-md", &fresh.to_string_lossy(), "--yes"], "", None);
    assert!(ok && fresh.join("CLAUDE.md").exists());
    let (ok, out) = scripted(&p, &["undo", "--yes"], "", None);
    assert!(ok && out.contains("atlas created it"), "{out}");
    assert!(!fresh.join("CLAUDE.md").exists());
}

/// Without a terminal (and without --yes) the offer asks nothing, so it
/// records nothing as offered: the next run in a terminal still asks.
#[test]
fn an_offer_without_a_terminal_marks_nothing_offered() {
    let h = Home::new("cs-noterm");
    let p = h.paths();
    let mut o = Vec::new();
    let mut i: &[u8] = b"";
    cs::run(&["offer".to_string()], &p, &mut Io { out: &mut o, input: &mut i, interactive: false, version: None, dry_run: false, machine: None }).unwrap();
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
    cs::run(&["offer".to_string()], &p, &mut Io { out: &mut o, input: &mut i, interactive: false, version: None, dry_run: false, machine: None }).unwrap();
    let o = String::from_utf8(o).unwrap();
    let line = o.lines().find(|l| l.starts_with("[update]")).unwrap_or_else(|| panic!("{o}"));
    assert!(atlas::update::release_build() || line.contains("off here: a build from source never updates itself"), "{line}");
}

/// The value at a dotted path of atlas's configuration.
fn config_at(h: &Home, path: &str) -> Option<json::Value> {
    let v = json::parse(&h.read(&h.config())).unwrap();
    path.split('.').try_fold(v, |v, k| v.get(k).cloned())
}

/// Orchestrated mode is asked, never defaulted: a terminal yes asks each
/// option (Enter keeps the value in brackets, a bad value is asked again)
/// and stores the answers; the limits are stored only when a number is
/// given, so their defaults keep scaling to the window.
#[test]
fn orchestrated_mode_is_asked_and_the_answers_are_stored() {
    let h = Home::new("cs-orch");
    let (ok, _) = h.run(&["list"]);
    assert!(ok);
    let template = h.read(&h.config());
    let p = h.paths();
    // No: nothing written, the answer recorded.
    let (ok, out) = scripted(&p, &["orchestrate"], "n\n", None);
    assert!(ok && out.contains("Work in orchestrated mode? [y/N]") && out.contains("not turned on; nothing written"), "{out}");
    assert_eq!(h.read(&h.config()), template);
    assert_eq!(cs::orchestrate_line(&p), None, "a no is recorded as asked");
    // Yes, with a bad cap asked again, an abbreviation, a number with commas.
    // The compile and gpu slots come first, so the caps follow them.
    let (ok, out) = scripted(&p, &["orchestrate"], "y\n\n\n500\n3\n\n0\na\n300,000\n500000\nk\n", None);
    assert!(ok, "{out}");
    assert!(out.contains("Compile slots (heavy compiles at once; 0 never serializes) [2]: ") && out.contains("GPU slots (GPU runs at once; 0 never serializes) [1]: "), "{out}");
    assert!(out.contains("Builders at once (agents that write and compile code) [2]: ") && out.contains("--builders: needs 0 to 100"), "{out}");
    assert!(out.contains("Reviewers at once (read-only review agents) [3]: "), "the reviewers follow the builders answered: {out}");
    assert!(out.contains("[285,000 on a 1M window, scaled to the window]"), "{out}");
    assert!(out.contains("[orchestrate] on: builders 3, reviewers 3, research 0, context limits advisory, handoff 300000, limit 500000, fresh ask, compile slots 2, gpu slots 1"), "{out}");
    for (k, v) in [
        ("orchestrate.enabled", "true"),
        ("orchestrate.resources.compile", "2"),
        ("orchestrate.resources.gpu", "1"),
        ("orchestrate.kinds.builder.max", "3"),
        ("orchestrate.kinds.reviewer.max", "3"),
        ("orchestrate.kinds.research.max", "0"),
        ("orchestrate.mode", "\"advisory\""),
        ("orchestrate.fresh", "\"ask\""),
        ("cap.warn_tokens", "300000"),
        ("cap.block_tokens", "500000"),
    ] {
        assert_eq!(config_at(&h, k).map(|x| x.to_string()).as_deref(), Some(v), "{k}");
    }
    // The resolver reads what was stored.
    let c = atlas::config::Config::load(&h.config()).unwrap();
    assert!(c.orchestrate.as_ref().and_then(|o| o.enabled) == Some(true), "the configuration parses");
    // Asked again: the brackets show the stored values, Enter keeps them,
    // a handoff above the limit is refused, and off skips both limits.
    let before = h.read(&h.config());
    let (ok, out) = scripted(&p, &["orchestrate"], "\n\n\n\n\n\n\n600000\n\n\n\n", None);
    assert!(ok && out.contains("[Y/n]") && out.contains("[3]: ") && out.contains("[advisory]: "), "{out}");
    assert!(out.contains("is above the hard limit"), "{out}");
    assert_eq!(h.read(&h.config()), before, "every answer kept: {out}");
    let (ok, out) = scripted(&p, &["orchestrate"], "y\n\n\n\n\n\noff\n\n", None);
    assert!(ok && !out.contains("Handoff point") && config_at(&h, "orchestrate.mode").unwrap() == "off", "{out}");
    // No while on turns the caps off; one undo per change brings the template back.
    let (ok, out) = scripted(&p, &["orchestrate"], "n\n", None);
    assert!(ok && config_at(&h, "orchestrate.enabled").unwrap() == false, "{out}");
    for _ in 0..3 {
        let (ok, out) = h.run(&["undo", "--yes"]);
        assert!(ok, "{out}");
    }
    assert_eq!(h.read(&h.config()), template);
    // The end of input, before an answer, writes nothing.
    let (ok, out) = scripted(&p, &["orchestrate"], "", None);
    assert!(ok && out.contains("no answer; nothing written"), "{out}");
    assert_eq!(h.read(&h.config()), template);
}

/// Without a terminal the binary lists the options and writes nothing;
/// `--yes` applies flags (checked as the questions are), `--no` declines.
#[test]
fn orchestrated_mode_answers_apply_without_a_terminal() {
    let h = Home::new("cs-orch-cli");
    let (ok, _) = h.run(&["list"]);
    assert!(ok);
    let template = h.read(&h.config());
    let (ok, out) = h.run(&["orchestrate"]);
    assert!(ok && out.contains("--builders Builders at once") && out.contains("--fresh") && out.contains("no terminal here to answer in"), "{out}");
    assert_eq!(h.read(&h.config()), template);
    for bad in [&["orchestrate", "--builders", "3"][..], &["orchestrate", "--yes", "--mode", "loud"], &["orchestrate", "--yes", "--handoff", "600000", "--limit", "500000"], &["orchestrate", "--no", "--research", "1"], &["orchestrate", "--yes", "--fresh", "later"]] {
        let (ok, out) = h.run(bad);
        assert!(!ok, "{bad:?}: {out}");
    }
    assert_eq!(h.read(&h.config()), template);
    let (ok, out) = h.run(&["set", "agents-research", "1"]);
    assert!(ok, "{out}");
    let (ok, out) = on_machine(&h.paths(), MACHINE, &["orchestrate", "--yes", "--builders", "3", "--limit", "500000"]);
    assert!(ok, "{out}");
    assert_eq!(config_at(&h, "orchestrate.kinds.builder.max").unwrap(), 3.0);
    assert_eq!(config_at(&h, "orchestrate.kinds.research.max").unwrap(), 1.0, "an unnamed option stores the value in effect");
    assert_eq!(config_at(&h, "orchestrate.resources.compile").unwrap(), 2.0, "a starter from the machine passed in (32 threads)");
    assert_eq!(config_at(&h, "orchestrate.mode").unwrap(), "enforced");
    assert_eq!(config_at(&h, "cap.block_tokens").unwrap(), 500000.0);
    assert!(config_at(&h, "cap.warn_tokens").is_none(), "the handoff point keeps scaling");
    let (ok, out) = h.run(&["orchestrate", "--no"]);
    assert!(ok && config_at(&h, "orchestrate.enabled").unwrap() == false, "{out}");
}

/// A run with no user (`claude -p`, the SDK) must not put the questions on its
/// stdout, and must not spend them: the next attended session still asks. What
/// Claude Code sets was measured on 2.1.319, interactive and print, in a console
/// and on pipes, nested and not.
#[test]
fn a_headless_session_neither_asks_nor_counts_as_asked() {
    let h = Home::new("cs-headless");
    let proj = h.t.path().join("work").join("demo");
    write(&proj.join("README.md"), "# Demo\n");
    let state = h.atlas().join("state").join(cs::OFFER_STATE);
    let asked = |c: &str| ["is not indexed", "orchestrated mode", "Claude Code setting", "On by default"].iter().filter(|w| c.contains(**w)).count();
    for session in [
        &[("CLAUDE_CODE_SESSION_ATTENDED", "0"), ("CLAUDE_CODE_ENTRYPOINT", "sdk-cli")][..],
        &[("CLAUDE_CODE_SESSION_ATTENDED", "0"), ("CLAUDE_CODE_ENTRYPOINT", "cli")],
        &[("CLAUDE_CODE_ENTRYPOINT", "sdk-cli")],
        &[("CLAUDE_CODE_ENTRYPOINT", "sdk-ts")],
        &[("CLAUDE_CODE_ENTRYPOINT", "sdk-py")],
    ] {
        let c = session_start_in(&h, &proj, session);
        assert_eq!(asked(&c), 0, "{session:?}: {c}");
        assert!(!state.exists(), "{session:?}: nothing counts as asked");
    }
    // Attended: all of it, and the word from Claude Code outranks the entry point.
    let attended = session_start_in(&h, &proj, &[("CLAUDE_CODE_SESSION_ATTENDED", "1"), ("CLAUDE_CODE_ENTRYPOINT", "sdk-cli")]);
    assert_eq!(asked(&attended), 4, "headless spent nothing: {attended}");
    let c = session_start_in(&h, &proj, &[("CLAUDE_CODE_ENTRYPOINT", "cli")]);
    assert!(c.contains("is not indexed") && c.contains("orchestrated mode"), "{c}");
    // Neither variable, as with a hook run by hand: it asks, as it always did.
    let c = session_start(&h, &proj);
    assert!(c.contains("is not indexed") && c.contains("orchestrated mode"), "{c}");
}

/// `atlas settings orchestrate ARGS` on a given machine, without a terminal.
fn on_machine(p: &Paths, m: cs::Machine, args: &[&str]) -> (bool, String) {
    let mut out = Vec::new();
    let mut input = &b""[..];
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let r = cs::run(&args, p, &mut Io { out: &mut out, input: &mut input, interactive: false, version: None, dry_run: false, machine: Some(m) });
    let text = String::from_utf8(out).unwrap();
    match r {
        Ok(code) => (code == std::process::ExitCode::SUCCESS, text),
        Err(e) => (false, format!("{text}{e}")),
    }
}

/// The numbers a yes stored: compile and gpu slots, then the three caps.
fn starters(h: &Home) -> [Option<f64>; 5] {
    ["orchestrate.resources.compile", "orchestrate.resources.gpu", "orchestrate.kinds.builder.max", "orchestrate.kinds.reviewer.max", "orchestrate.kinds.research.max"].map(|k| config_at(h, k).and_then(|v| v.as_f64()))
}

/// A new orchestrated install ends with declared starter resources and
/// kinds, derived from the machine passed in (compile = max(1, cores / 16)
/// at most 4; gpu 1; builders = compile, reviewers = builders, research =
/// max(1, builders / 2), each clamped to Claude Code's limit). A configured
/// value and a flag win, `--no` writes none, and no limit or handoff is
/// written without a flag.
#[test]
fn a_new_orchestrated_install_declares_starter_resources_and_kinds() {
    let m = |cores: usize, claude_limit: usize| cs::Machine { cores, claude_limit };
    for (tag, machine, want) in [
        ("cs-start-2", m(2, 20), [1.0, 1.0, 1.0, 1.0, 1.0]),
        ("cs-start-16", m(16, 20), [1.0, 1.0, 1.0, 1.0, 1.0]),
        ("cs-start-32", m(32, 20), [2.0, 1.0, 2.0, 2.0, 1.0]),
        ("cs-start-64", m(64, 20), [4.0, 1.0, 4.0, 4.0, 2.0]),
        ("cs-start-256", m(256, 3), [4.0, 1.0, 3.0, 3.0, 1.0]),
    ] {
        let h = Home::new(tag);
        let (ok, _) = h.run(&["list"]);
        assert!(ok);
        let (ok, out) = on_machine(&h.paths(), machine, &["orchestrate", "--yes"]);
        assert!(ok, "{tag}: {out}");
        assert_eq!(starters(&h), want.map(Some), "{tag}: {out}");
        for k in ["cap.warn_tokens", "cap.block_tokens", "orchestrate.kinds.builder.limit", "orchestrate.kinds.builder.handoff", "orchestrate.kinds.reviewer.limit", "orchestrate.kinds.research.handoff"] {
            assert!(config_at(&h, k).is_none(), "{tag}: {k} is not written without a flag");
        }
        assert_eq!(cs::starter_compile(machine.cores) as f64, want[0], "{tag}");
    }
    // A configured value is kept, and the caps follow a configured compile count.
    let h = Home::new("cs-start-kept");
    for args in [&["set", "agents-builders", "6"][..], &["set", "resources-compile", "3"]] {
        let (ok, out) = h.run(args);
        assert!(ok, "{args:?}: {out}");
    }
    let (ok, out) = on_machine(&h.paths(), m(64, 20), &["orchestrate", "--yes"]);
    assert!(ok, "{out}");
    assert_eq!(starters(&h), [Some(3.0), Some(1.0), Some(6.0), Some(6.0), Some(3.0)], "{out}");
    // Flags win over the starters, and the caps follow an answered compile count.
    let h = Home::new("cs-start-flags");
    let (ok, _) = h.run(&["list"]);
    assert!(ok);
    let (ok, out) = on_machine(&h.paths(), m(2, 20), &["orchestrate", "--yes", "--compile", "3", "--gpu", "0", "--research", "4"]);
    assert!(ok, "{out}");
    assert_eq!(starters(&h), [Some(3.0), Some(0.0), Some(3.0), Some(3.0), Some(4.0)], "{out}");
    let (ok, out) = on_machine(&h.paths(), m(2, 20), &["orchestrate", "--yes", "--compile", "65"]);
    assert!(!ok && out.contains("needs 0 to 64"), "{out}");
    // `--no` on a new install writes no resources or kinds.
    let h = Home::new("cs-start-no");
    let (ok, _) = h.run(&["list"]);
    assert!(ok);
    let template = h.read(&h.config());
    let (ok, out) = on_machine(&h.paths(), m(64, 20), &["orchestrate", "--no"]);
    assert!(ok, "{out}");
    assert_eq!(h.read(&h.config()), template);
    assert_eq!(starters(&h), [None; 5]);
}

/// The first session asks through Claude once, naming every option and
/// both answers; never when the switch is already set.
#[test]
fn the_first_session_offers_orchestrated_mode_once() {
    let h = Home::new("cs-orch-line");
    let (ok, _) = h.run(&["list"]);
    assert!(ok);
    let p = h.paths();
    let line = cs::orchestrate_line(&p).unwrap();
    for w in ["In your first reply, whatever the task, ask the user whether to work in orchestrated mode", "--yes --mode MODE --fresh auto|ask", "--compile N --gpu N --builders N --reviewers N --research N", "from this machine's cores", "--handoff N --limit N", "285k and 450k", "atlas settings orchestrate --no", "or no user to ask, run nothing"] {
        assert!(line.contains(w), "{w}: {line}");
    }
    assert!(!line.contains("2, 2, 1"), "the line names the rule, not numbers: {line}");
    assert!(cs::orchestrate_line(&p).is_some(), "still unanswered, so it returns");
    let (ok, out) = h.run(&["orchestrate", "--no"]);
    assert!(ok, "{out}");
    assert_eq!(cs::orchestrate_line(&p), None, "answered, so never repeated");
    let h2 = Home::new("cs-orch-line2");
    let (ok, out) = h2.run(&["set", "orchestrate", "off"]);
    assert!(ok, "{out}");
    assert_eq!(cs::orchestrate_line(&h2.paths()), None, "already answered in the configuration");
    // The setup offer asks it too, after the Claude Code settings.
    let h3 = Home::new("cs-orch-offer");
    let (ok, _) = h3.run(&["list"]);
    assert!(ok);
    let (ok, out) = scripted(&h3.paths(), &["offer"], "n\nn\ny\n\n\n\n\n\n\n\n", Some((2, 1, 316)));
    assert!(ok && out.contains("Work in orchestrated mode?"), "{out}");
    assert_eq!(config_at(&h3, "orchestrate.enabled").unwrap(), true);
    assert_eq!(cs::orchestrate_line(&h3.paths()), None);
}

/// The live hook drift check (journal ATLAS-V03:0137, review 0141): the
/// atlas hooks of a user's settings.json are compared with hooks/hooks.json
/// per event (an event with no atlas hook, and the input fields of an
/// mcp_tool hook), by `atlas doctor` and at a session's first prompt.
#[test]
fn live_atlas_hooks_are_checked_against_hooks_json() {
    use atlas::hooks::{HookEvent, Hooks};
    // The plugin's own file has no drift; neither has a file without atlas hooks.
    let plugin = json::parse(cs::PLUGIN_HOOKS).unwrap();
    assert_eq!(cs::hook_drift(&plugin), Vec::<String>::new());
    assert_eq!(cs::hook_drift(&json::parse(r#"{"hooks": {"PostToolUse": [{"hooks": [{"type": "command", "command": "node x.js"}]}]}}"#).unwrap()), Vec::<String>::new());
    // A hand-installed setup: the plugin's hooks under the
    // server name `atlas`, SessionStart as the installed binary's command hook.
    let same = cs::PLUGIN_HOOKS.replace("plugin:atlas:atlas", "atlas").replace(r#""\"${CLAUDE_PLUGIN_ROOT}/bin/atlas\" --hook""#, r#""\"/opt/atlas/bin/atlas\" --hook""#);
    assert!(same.contains("/opt/atlas/bin/atlas"), "{same}");
    assert_eq!(cs::hook_drift(&json::parse(&same).unwrap()), Vec::<String>::new());
    // Before: four mcp_tool events only, PreToolUse without tool_use_id and
    // cwd, and a PreCompact field the plugin does not send.
    let mut before = json::parse(&same).unwrap();
    if let Some(h) = before["hooks"].as_object_mut() {
        for e in ["SessionStart", "SubagentStop", "PostToolUse"] {
            h.remove(e);
        }
    }
    let before = before
        .to_string()
        .replace(r#""tool_use_id":"${tool_use_id}","#, "")
        .replace(r#""cwd":"${cwd}","#, "")
        .replace(r#""trigger":"${trigger}","#, r#""trigger":"${trigger}","source":"${source}","#);
    let want = ["SessionStart: no atlas hook", "SubagentStop: no atlas hook", "PreToolUse: missing cwd, tool_use_id", "PostToolUse: no atlas hook", "PreCompact: extra source"];
    assert_eq!(cs::hook_drift(&json::parse(&before).unwrap()), want, "{before}");
    // An atlas hook on an event the plugin does not hook.
    let mut odd = json::parse(&same).unwrap();
    let stop = odd["hooks"]["SubagentStop"].clone();
    if let Some(h) = odd["hooks"].as_object_mut() {
        h.insert("Notification".into(), stop);
    }
    assert_eq!(cs::hook_drift(&odd), ["Notification: an atlas hook on an event hooks/hooks.json does not hook"]);
    // A legacy setup routing a recalling event to `atlas --hook` reloads the
    // model per prompt; a command hook on another event is not said.
    let mut legacy = json::parse(&same).unwrap();
    let command = legacy["hooks"]["SessionStart"].clone();
    if let Some(h) = legacy["hooks"].as_object_mut() {
        h.insert("UserPromptSubmit".into(), command.clone());
        h.insert("PreCompact".into(), command.clone());
    }
    let finding = "a command hook starts a new process per event that reloads the model and record vectors; use the mcp_tool hook";
    assert_eq!(cs::hook_drift(&legacy), [format!("UserPromptSubmit: {finding}")]);
    // SubagentStart recalls too: said on its own, and beside UserPromptSubmit.
    let mut sub = json::parse(&same).unwrap();
    if let Some(h) = sub["hooks"].as_object_mut() {
        h.insert("SubagentStart".into(), command.clone());
    }
    assert_eq!(cs::hook_drift(&sub), [format!("SubagentStart: {finding}")]);
    if let Some(h) = legacy["hooks"].as_object_mut() {
        h.insert("SubagentStart".into(), command);
    }
    assert_eq!(cs::hook_drift(&legacy), [format!("UserPromptSubmit: {finding}"), format!("SubagentStart: {finding}")]);

    let h = Home::new("cs-drift");
    let doctor = || {
        let o = Command::new(bin()).arg("doctor").env("CLAUDE_CONFIG_DIR", h.claude()).env_remove("ATLAS_CONFIG").output().unwrap();
        (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    std::fs::write(h.settings(), &same).unwrap();
    let (ok, out) = doctor();
    assert!(ok && out.contains("match hooks/hooks.json"), "{out}");
    std::fs::write(h.settings(), &before).unwrap();
    let (ok, out) = doctor();
    assert!(!ok && want.iter().all(|w| out.contains(w)), "{out}");

    // A session's first prompt carries the warning; later prompts do not;
    // a new session hears it again.
    let s = atlas::memory::settings::Settings::from_config(None, &h.atlas().join("state").join("index.sqlite")).unwrap();
    let mut hooks = Hooks::open(s).unwrap();
    hooks.set_offer_paths(Some(h.paths()));
    let prompt = |hooks: &mut Hooks, session: &str| hooks.handle(&HookEvent { event: "UserPromptSubmit".into(), session_id: session.into(), prompt: "what next?".into(), ..HookEvent::default() }).context;
    let first = prompt(&mut hooks, "s1");
    assert!(first.contains("differ from the plugin's hooks/hooks.json (SessionStart: no atlas hook; SubagentStop: no atlas hook;"), "{first}");
    assert!(!prompt(&mut hooks, "s1").contains("hooks/hooks.json"));
    assert!(prompt(&mut hooks, "s2").contains("hooks/hooks.json"));
    std::fs::write(h.settings(), &same).unwrap();
    assert!(!prompt(&mut hooks, "s3").contains("hooks/hooks.json"));
}

/// A legacy setup that routes every atlas hook to `atlas --hook` starts a
/// new process per prompt, so a prompt cannot tell the drift once: no prompt
/// tells it, and a new or cleared session's SessionStart tells it once. A
/// resident setup (an mcp_tool UserPromptSubmit) still hears it at the first
/// prompt only, and not at SessionStart as well.
#[test]
fn a_command_hook_setup_hears_the_drift_once_at_session_start() {
    use atlas::hooks::{HookEvent, Hooks};
    let h = Home::new("cs-drift-command");
    let same = cs::PLUGIN_HOOKS.replace("plugin:atlas:atlas", "atlas").replace(r#""\"${CLAUDE_PLUGIN_ROOT}/bin/atlas\" --hook""#, r#""\"/opt/atlas/bin/atlas\" --hook""#);
    let mut legacy = json::parse(&same).unwrap();
    let command = legacy["hooks"]["SessionStart"].clone();
    if let Some(hooks) = legacy["hooks"].as_object_mut() {
        let events: Vec<String> = hooks.keys().cloned().collect();
        for e in events {
            hooks.insert(e, command.clone());
        }
    }
    std::fs::write(h.settings(), legacy.to_string()).unwrap();
    let s = atlas::memory::settings::Settings::from_config(None, &h.atlas().join("state").join("index.sqlite")).unwrap();
    // Each `atlas --hook` run is a fresh state.
    let run = |event: &str, source: &str, command: bool| {
        let mut hooks = Hooks::open(s.clone()).unwrap();
        if command {
            hooks.set_command_hook();
        }
        hooks.set_offer_paths(Some(h.paths()));
        hooks.handle(&HookEvent { event: event.into(), session_id: "s1".into(), source: source.into(), prompt: "what next?".into(), ..HookEvent::default() }).context
    };
    let told = |ctx: &str| ctx.matches("differ from the plugin's hooks/hooks.json").count();
    for _ in 0..2 {
        let ctx = run("UserPromptSubmit", "", true);
        assert_eq!(told(&ctx), 0, "a prompt in a new process: {ctx}");
    }
    let start = run("SessionStart", "startup", true);
    assert_eq!(told(&start), 1, "{start}");
    assert!(start.contains("UserPromptSubmit: a command hook starts a new process per event that reloads the model and record vectors"), "{start}");
    assert!(!start.contains("tool_use_id"), "the command finding has its own reason: {start}");
    assert_eq!(told(&run("SessionStart", "clear", true)), 1);
    for source in ["resume", "compact"] {
        let ctx = run("SessionStart", source, true);
        assert_eq!(told(&ctx), 0, "{source}: the same session: {ctx}");
    }
    // Resident: the plugin's hooks with SubagentStart as a command hook.
    let mut resident = json::parse(&same).unwrap();
    if let Some(hooks) = resident["hooks"].as_object_mut() {
        hooks.insert("SubagentStart".into(), command);
    }
    std::fs::write(h.settings(), resident.to_string()).unwrap();
    assert_eq!(told(&run("SessionStart", "startup", true)), 0, "the first prompt tells it");
    let mut hooks = Hooks::open(s.clone()).unwrap();
    hooks.set_offer_paths(Some(h.paths()));
    let mut prompt = || hooks.handle(&HookEvent { event: "UserPromptSubmit".into(), session_id: "s1".into(), prompt: "what next?".into(), ..HookEvent::default() }).context;
    let first = prompt();
    assert_eq!(told(&first), 1, "{first}");
    assert!(first.contains("SubagentStart: a command hook"), "{first}");
    assert_eq!(told(&prompt()), 0);
}

/// `atlas doctor` says what it checked: "match" only for real hooks that match,
/// and each case that checked nothing has its own line. S5 of the v0.3 audit.
#[test]
fn doctor_says_what_it_checked() {
    let h = Home::new("cs-doctor");
    let settings = h.settings();
    let doctor = |h: &Home| {
        let o = Command::new(bin()).arg("doctor").env("CLAUDE_CONFIG_DIR", h.claude()).env("USERPROFILE", h.claude()).env("HOME", h.claude()).env("ATLAS_CONFIG", h.claude().join("no-config.json")).output().unwrap();
        (o.status.code(), String::from_utf8_lossy(&o.stdout).trim_end().to_string())
    };
    let shown = settings.display().to_string();
    // No file.
    let _ = std::fs::remove_file(&settings);
    assert_eq!(doctor(&h), (Some(0), format!("hooks: there is no {shown}, so there was nothing there to check")));
    // Malformed: not a match, and exit 1.
    std::fs::write(&settings, "{broken").unwrap();
    let (code, out) = doctor(&h);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.starts_with(&format!("hooks: not checked, the settings file could not be read: {shown}: not JSON")), "{out}");
    assert!(!out.contains("match"), "{out}");
    // No atlas hooks in it; an empty file reads the same.
    std::fs::write(&settings, r#"{"hooks": {"PostToolUse": [{"hooks": [{"type": "command", "command": "node x.js"}]}]}}"#).unwrap();
    assert_eq!(doctor(&h), (Some(0), format!("hooks: {shown} holds no atlas hooks, so there was nothing there to check")));
    std::fs::write(&settings, "").unwrap();
    assert_eq!(doctor(&h), (Some(0), format!("hooks: {shown} holds no atlas hooks, so there was nothing there to check")));
    // A plugin install, in the shape Claude Code writes.
    let plugins = h.claude().join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let list = plugins.join("installed_plugins.json");
    std::fs::write(&list, r#"{"version": 2, "plugins": {"atlas@atlas": [{"scope": "user", "version": "0.2.1"}]}}"#).unwrap();
    let plugin_line = "plugin: atlas 0.2.1 is installed as a plugin; its hooks are in the plugin and were not checked here";
    let (code, out) = doctor(&h);
    assert_eq!((code, out.lines().last()), (Some(0), Some(plugin_line)), "{out}");
    assert!(!out.contains("match"), "{out}");
    // Someone else's plugin is no atlas install.
    std::fs::write(&list, r#"{"version": 2, "plugins": {"atlas-tools@x": [{"version": "1"}]}}"#).unwrap();
    assert!(!doctor(&h).1.contains("plugin:"));
    // An install list that cannot be read cannot say either way.
    std::fs::write(&list, "{broken").unwrap();
    let (code, out) = doctor(&h);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.lines().last().unwrap().starts_with("plugin: cannot tell whether atlas is installed as a plugin: "), "{out}");
    std::fs::remove_file(&list).unwrap();
    // Real hooks that match are the only "match".
    let same = cs::PLUGIN_HOOKS.replace("plugin:atlas:atlas", "atlas").replace(r#""\"${CLAUDE_PLUGIN_ROOT}/bin/atlas\" --hook""#, r#""\"/opt/atlas/bin/atlas\" --hook""#);
    std::fs::write(&settings, &same).unwrap();
    assert_eq!(doctor(&h), (Some(0), format!("hooks: the atlas hooks in {shown} match hooks/hooks.json")));
}

/// `atlas doctor` names a root that an older setup holds, rewrites nothing and
/// makes no configuration of its own. S2 of the v0.3 audit.
#[test]
fn doctor_names_a_project_root_that_is_too_wide() {
    let h = Home::new("cs-doctor-roots");
    let home = h.claude().join("home");
    std::fs::create_dir_all(home.join("code")).unwrap();
    let cfg = h.claude().join("wide.json");
    let run = |cfg: &std::path::Path| {
        let o = Command::new(bin()).arg("doctor").env("CLAUDE_CONFIG_DIR", h.claude()).env("USERPROFILE", &home).env("HOME", &home).env("ATLAS_CONFIG", cfg).output().unwrap();
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let text = format!(r#"{{"projects": [{{"name": "wide", "roots": ["{}"]}}, {{"name": "fine", "roots": ["{}"]}}]}}"#, home.display().to_string().replace('\\', "/"), home.join("code").display().to_string().replace('\\', "/"));
    std::fs::write(&cfg, &text).unwrap();
    let (code, out) = run(&cfg);
    assert_eq!(code, Some(0), "named, not a failure: {out}");
    assert!(out.contains("roots: project wide in "), "{out}");
    assert!(!out.contains("project fine"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text, "nothing rewritten");
    // No configuration: nothing is written, and nothing is said about roots.
    let none = h.claude().join("absent").join("atlas.json");
    let (_, out) = run(&none);
    assert!(!out.contains("roots:"), "{out}");
    assert!(!none.parent().unwrap().exists(), "the doctor writes nothing");
    // A project file at home claims every folder below it.
    std::fs::write(home.join("atlas.project.json"), r#"{"name": "x"}"#).unwrap();
    let (_, out) = run(&none);
    assert!(out.contains("roots: the project file "), "{out}");
}

/// After a failed start Claude Code holds the server off for 15 minutes, which a
/// user chasing a typo reads as the fix not working. The doctor names it, read only.
#[test]
fn doctor_names_a_server_claude_code_is_holding_off() {
    let h = Home::new("cs-doctor-held");
    let run = || {
        let o = Command::new(bin()).arg("doctor").env("CLAUDE_CONFIG_DIR", h.claude()).env("USERPROFILE", h.claude()).env("HOME", h.claude()).env("ATLAS_CONFIG", h.claude().join("none.json")).output().unwrap();
        (o.status.code(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let file = h.claude().join("mcp-needs-auth-cache.json");
    // No file says nothing.
    assert!(!run().1.contains("server:"));
    let text = format!(r#"{{"plugin:atlas:atlas": {{"timestamp": {}, "id": "x"}}, "atlas": {{"timestamp": {}, "id": "y"}}, "blender": {{"timestamp": {now}, "id": "z"}}}}"#, now - 60_000, now - 20 * 60_000);
    std::fs::write(&file, &text).unwrap();
    let (code, out) = run();
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("server: Claude Code recorded a failed start of plugin:atlas:atlas") && out.contains("about 14 more minutes"), "{out}");
    assert!(out.contains("unless the fix changed its command"), "{out}");
    assert_eq!(out.matches("server:").count(), 1, "an expired entry and another server's say nothing: {out}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "read only");
    // What is not understood says nothing.
    std::fs::write(&file, "{broken").unwrap();
    assert!(!run().1.contains("server:"));
}

/// What the state file records as offered.
fn offered_ids(h: &Home) -> String {
    std::fs::read_to_string(h.atlas().join("state").join(cs::OFFER_STATE)).unwrap_or_default()
}

/// How a session starts, through the hook that tells the agent what to ask.
fn first_session_context(h: &Home) -> String {
    use atlas::hooks::{HookEvent, Hooks};
    let s = atlas::memory::settings::Settings::from_config(None, &h.atlas().join("state").join("index.sqlite")).unwrap();
    let mut hooks = Hooks::open(s).unwrap();
    hooks.set_offer_paths(Some(h.paths()));
    hooks.handle(&HookEvent { event: "SessionStart".into(), session_id: "s1".into(), source: "startup".into(), ..HookEvent::default() }).context
}

#[test]
fn memory_rules_is_never_asked() {
    let h = Home::new("settings-mr-silent");
    let cfg = h.config();
    // No terminal: not described, and nothing written.
    let (ok, out) = h.run(&["offer"]);
    assert!(ok && !out.contains("memory-rules") && !out.contains("Memory and rule files"), "{out}");
    assert!(!h.read(&cfg).contains("\"rules\""));
    // A session: no line, and the id is not recorded as offered.
    let context = first_session_context(&h);
    assert!(!context.contains("memory folder") && !context.contains("memory-rules"), "{context}");
    assert!(!offered_ids(&h).contains("memory-rules"), "{}", offered_ids(&h));
    // A terminal: two settings and orchestrated mode take their answers, the
    // next line is left for whatever pipes it, and nothing is written.
    let before = h.read(&cfg);
    let mut out = Vec::new();
    let mut input: &[u8] = b"n\nn\nn\nleft alone\n";
    let code = cs::run(&["offer".to_string()], &h.paths(), &mut Io { out: &mut out, input: &mut input, interactive: true, version: None, dry_run: false, machine: None }).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(code == std::process::ExitCode::SUCCESS && !out.contains("Should atlas manage") && !out.contains("memory-rules"), "{out}");
    assert_eq!(input, b"left alone\n", "{out}");
    assert_eq!(h.read(&cfg), before, "{out}");
    // Marked offered like the rest, apart from this id.
    let state = offered_ids(&h);
    assert!(state.contains("subagent-cache-1h") && !state.contains("memory-rules"), "{state}");
    // Named by hand it is still refused nothing, and still writes nothing.
    let (ok, out) = h.run(&["offer", "--yes", "memory-rules"]);
    assert!(ok, "{out}");
    assert_eq!(h.read(&cfg), before, "{out}");
    assert!(!offered_ids(&h).contains("memory-rules"), "{}", offered_ids(&h));
}

#[test]
fn off_returns_memory_rules_to_unset() {
    let h = Home::new("settings-mr-off");
    let cfg = h.config();
    let (ok, out) = h.run(&["list"]);
    assert!(ok && out.contains("[memory-rules] Experimental, off by default"), "{out}");
    let before = h.read(&cfg);
    assert!(!before.contains("\"rules\""), "{before}");
    // The `memory` block `set` made may stay, empty: a later test shows that is
    // no block at all.
    let unset = || h.read(&cfg).replace(",
  \"memory\": {}", "");
    // Unset already: off has nothing to do.
    let (ok, out) = h.run(&["off", "memory-rules"]);
    assert!(ok && out.contains("already off"), "{out}");
    assert_eq!(h.read(&cfg), before);
    // On, deliberately, and back: the same bytes, and list reads the default.
    let (ok, out) = h.run(&["set", "memory-rules", "true"]);
    assert!(ok && out.contains("experimental"), "{out}");
    assert!(h.read(&cfg).contains("\"rules\": true"));
    let (ok, out) = h.run(&["off", "memory-rules"]);
    assert!(ok && out.contains("[memory-rules] removed"), "{out}");
    assert_eq!(unset(), before, "{out}");
    let (ok, out) = h.run(&["list"]);
    assert!(ok && out.contains("atlas default: unset"), "{out}");
    // False is the same as unset, says so, and off removes it.
    for word in ["false", "FALSE", "no", "0"] {
        let (ok, out) = h.run(&["set", "memory-rules", word]);
        assert!(ok && out.contains("false is the same as unset"), "{word}: {out}");
        assert!(h.read(&cfg).contains("\"rules\": false"), "{word}: {out}");
        let (ok, out) = h.run(&["off", "memory-rules"]);
        assert!(ok && out.contains("removed"), "{word}: {out}");
        assert_eq!(unset(), before, "{word}: {out}");
    }
    // The word off, in any case, is the full off: it writes "off", says what it
    // stops beyond unset, and off removes it too.
    for word in ["off", "OFF"] {
        let (ok, out) = h.run(&["set", "memory-rules", word]);
        assert!(ok && out.contains("also stops") && out.contains("names by hand"), "{word}: {out}");
        assert!(h.read(&cfg).contains("\"rules\": \"off\""), "{word}: {out}");
        let (ok, out) = h.run(&["list"]);
        assert!(ok && out.contains("also stops sync and query recall"), "{out}");
        let (ok, out) = h.run(&["off", "memory-rules"]);
        assert!(ok && out.contains("removed"), "{word}: {out}");
        assert_eq!(unset(), before, "{word}: {out}");
    }
    let (ok, out) = h.run(&["set", "memory-rules", "on"]);
    assert!(ok && out.contains("warning: experimental"), "{out}");
    // Another setting's warning still reads by its value.
    let (ok, out) = h.run(&["set", "goal-checkin-120", "0"]);
    assert!(ok && out.contains("warning: 0 turns check-ins off"), "{out}");
}

/// Off on a configuration that had no `memory` block: what it leaves is no block
/// at all, whole, for the flat form and for a project's.
#[test]
fn off_on_a_config_with_no_memory_block_leaves_it_as_absent() {
    let h = Home::new("settings-mr-noblock");
    let cfg = h.config();
    let (ok, out) = h.run(&["list"]);
    assert!(ok, "{out}");
    let root = cfg.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(root.join("repo")).unwrap();
    std::fs::create_dir_all(root.join("notes")).unwrap();
    let flat = "{\n  \"watch\": false\n}\n";
    let scoped = "{\n  \"watch\": false,\n  \"projects_dir\": \"state\",\n  \"projects\": [{\"name\": \"p\", \"roots\": [\"repo\"], \"vaults\": [{\"name\": \"notes\", \"path\": \"notes\"}]}]\n}\n";
    for bare in [flat, scoped] {
        std::fs::write(&cfg, bare).unwrap();
        let (ok, out) = h.run(&["set", "memory-rules", "true"]);
        assert!(ok && h.read(&cfg).contains("\"rules\": true"), "{out}");
        let (ok, out) = h.run(&["off", "memory-rules"]);
        assert!(ok, "{out}");
        let after = h.read(&cfg);
        assert!(!after.contains("\"rules\""), "{after}");
        let parsed = atlas::config::Config::load(&cfg).unwrap_or_else(|e| panic!("{e}: {after}"));
        assert_eq!(format!("{:?}", parsed.memory.clone().unwrap_or_default()), format!("{:?}", atlas::config::MemorySpec::default()), "{after}");
        let absent_at = cfg.with_file_name("bare.json");
        std::fs::write(&absent_at, bare).unwrap();
        let absent = atlas::config::Config::load(&absent_at).unwrap();
        // What every reader takes it for, through the project's resolution.
        let settings = |c: &atlas::config::Config| {
            let e = atlas::project::resolve(c, &root.join("repo"), None).unwrap().config;
            atlas::memory::settings::Settings::from_config(Some(&e), &root.join("state").join("index.sqlite")).unwrap()
        };
        assert_eq!(settings(&parsed), settings(&absent), "{after}");
    }
}
