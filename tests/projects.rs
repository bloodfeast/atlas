//! Projects: one install, scoped projects (see `src/project.rs`).
//!
//! * No bleed: two projects never see each other's records through recall,
//!   the hooks, the journal, the decision queue or `next_id`, and both see
//!   the user layer.
//! * Migration: a configuration without projects becomes one project with
//!   its database and memory store where they were, once.
//! * Multi-repo: a root folder of git repositories with no project file is
//!   discovered and offered once; the project file is written (and undone);
//!   a repository listed by remote is found through the checkout map;
//!   routing follows the paths a session touches; `.proto` symbols answer
//!   who defines a message and who calls an rpc across repositories.
//!
//! Every name here is synthetic.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use atlas::claude_settings::{self as cs, Io, OfferCtx, Paths};
use atlas::config::Config;
use atlas::hooks::{HookEvent, Hooks};
use atlas::json;
use atlas::json::Value;
use atlas::memory::settings::Settings;
use atlas::project::{self, Why};
use atlas::tools::Server;
use common::*;

/// A server over a resolved configuration, as `main` builds one.
fn server_for(c: &Config) -> Server {
    let db = c.db.clone().unwrap();
    let vaults: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
    let mut setup = atlas::store::AtlasSetup::from_config(c).unwrap();
    setup.watch = false;
    let mut store = atlas::store::Store::open_with(&db, atlas::paths::Vaults::new(&vaults).unwrap(), setup).unwrap();
    store.rescan().unwrap();
    let mut s = Server::new(store);
    s.set_client("test-client");
    s.set_memory(settings_for(c));
    s
}

fn settings_for(c: &Config) -> Settings {
    let mut s = Settings::from_config(Some(c), c.db.as_ref().unwrap()).unwrap();
    s.snapshot_secs = 0;
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    s
}

fn text(s: &mut Server, tool: &str, args: Value) -> String {
    let a = if matches!(tool, "remember") {
        let mut a = args;
        if a.get("reason").is_none() {
            a["reason"] = json!("projects test: a synthetic write");
        }
        a
    } else {
        args
    };
    // Every answer whole: these tests read the store again after changing
    // it, which a repeat would answer with the changed lines.
    let mut a = a;
    a["fresh"] = json!(true);
    match s.answer(tool, &a) {
        Ok(x) => x.into_text(),
        Err(e) => panic!("{tool}: {e}"),
    }
}

fn memory_file(dir: &Path, key: &str, desc: &str, body: &str, extra: &str) {
    write(&dir.join(format!("{key}.md")), &format!("---\nname: {key}\ndescription: {desc}\ntype: feedback\n{extra}---\n\n{body}\n"));
}

/// Two projects, a user memory folder, and the configuration naming them.
struct Two {
    t: Temp,
}

impl Two {
    fn new(tag: &str) -> Two {
        let t = Temp::new(tag);
        let r = t.path();
        let um = r.join("user-memory");
        memory_file(&um, "kettle-descaling", "descale the kettle every month", "Descale the kettle every month with citric acid.", "");
        write(&um.join("MEMORY.md"), "- [Kettle descaling](kettle-descaling.md) - descale the kettle every month\n");
        for (p, key, body) in [("alpha", "alpha-widget", "The alpha widget compactor runs nightly at two."), ("beta", "beta-flux", "The beta flux capacitor drains at noon.")] {
            let m = r.join(p).join("memory");
            memory_file(&m, key, &format!("{p} private fact"), body, "");
            write(&m.join("MEMORY.md"), &format!("- [{key}]({key}.md) - {p} private fact\n"));
            write(&r.join(p).join("vault").join("Ideas").join("seed.md"), "# Seed\n\nA seed note.\n");
        }
        // A user-level memory kept in alpha's folder, marked for every project.
        memory_file(&r.join("alpha").join("memory"), "tabs-not-spaces", "indent with tabs everywhere", "Indent with tabs in every repository.", "scope: user\n");
        write(&r.join("alpha").join("INTENT.md"), "# Rules\n\n- 2026-11-01 Dana: the alpha gizmo exporter writes temp files, then renames them.\n");
        write(
            &r.join("atlas.json"),
            r#"{
  "projects_dir": "state",
  "user": {"memory": "user-memory"},
  "projects": [
    {"name": "alpha", "roots": ["alpha"], "memory_dir": "alpha/memory", "intent": "alpha/INTENT.md", "vaults": [{"name": "notes", "path": "alpha/vault"}]},
    {"name": "beta", "roots": ["beta"], "memory_dir": "beta/memory", "vaults": [{"name": "notes", "path": "beta/vault"}]}
  ],
  "watch": false
}"#,
        );
        Two { t }
    }
    fn root(&self) -> &Path {
        self.t.path()
    }
    fn config(&self) -> Config {
        Config::load(&self.root().join("atlas.json")).unwrap()
    }
    fn resolve(&self, p: &str) -> project::Resolved {
        project::resolve(&self.config(), &self.root().join(p).join("vault"), None).unwrap()
    }
}

#[test]
fn projects_never_see_each_others_records_and_both_see_the_user_layer() {
    let two = Two::new("proj-bleed");
    let (ra, rb) = (two.resolve("alpha"), two.resolve("beta"));
    assert_eq!(ra.active.as_ref().map(|a| (a.spec.name.as_str(), a.why)), Some(("alpha", Why::Root)));
    assert_eq!(rb.active.as_ref().map(|a| a.spec.name.as_str()), Some("beta"));
    let (ca, cb) = (ra.config, rb.config);
    assert_ne!(ca.db, cb.db, "each project has its own database");
    assert_eq!(ca.db.as_deref(), Some(two.root().join("state").join("alpha").join("atlas.sqlite").as_path()));
    let (mut a, mut b) = (server_for(&ca), server_for(&cb));

    // Recall: the project's own memory, the user layer, never the other's.
    let ra = text(&mut a, "recall", json!({"query": "widget compactor nightly"}));
    assert!(ra.contains("alpha-widget"), "{ra}");
    let rb = text(&mut b, "recall", json!({"query": "widget compactor nightly"}));
    assert!(!rb.contains("alpha-widget"), "beta recalled alpha's memory: {rb}");
    assert!(!text(&mut a, "recall", json!({"query": "flux capacitor noon"})).contains("beta-flux"));
    for s in [&mut a, &mut b] {
        let k = text(s, "recall", json!({"key": "kettle-descaling"}));
        assert!(k.contains("citric acid"), "the user layer is in every project: {k}");
        let t = text(s, "recall", json!({"key": "tabs-not-spaces"}));
        assert!(t.contains("Indent with tabs"), "a memory marked scope: user is in every project: {t}");
    }
    let rule = text(&mut b, "recall", json!({"query": "gizmo exporter temp files renames"}));
    assert!(!rule.contains("gizmo"), "beta recalled alpha's rule: {rule}");
    assert!(text(&mut a, "recall", json!({"query": "gizmo exporter temp files renames"})).contains("gizmo"));

    // The hooks read the project's store only.
    for (c, own, other) in [(&ca, "alpha-widget", "beta-flux"), (&cb, "beta-flux", "alpha-widget")] {
        let mut h = Hooks::open(settings_for(c)).unwrap();
        h.set_offer_paths(None);
        let prompt = "the alpha widget compactor runs nightly while the beta flux capacitor drains at noon";
        let ans = h.handle(&HookEvent { event: "UserPromptSubmit".into(), session_id: "s1".into(), prompt: prompt.into(), ..HookEvent::default() });
        assert!(ans.context.contains(own), "{own} not injected: {}", ans.context);
        assert!(!ans.context.contains(other), "{other} bled in: {}", ans.context);
    }

    // Lanes and the journal.
    text(&mut a, "journal", json!({"lane": "TASK-42", "entry": "note", "text": "alpha lane work on the compactor"}));
    assert!(text(&mut a, "journal_status", json!({})).contains("TASK-42"));
    let js = text(&mut b, "journal_status", json!({}));
    assert!(!js.contains("TASK-42"), "beta lists alpha's lane: {js}");
    assert!(!text(&mut b, "recall", json!({"lane": "TASK-42"})).contains("compactor"));

    // next_id: the same vault name, a separate sequence per project.
    let id = |s: &mut Server| call(s, "next_id", json!({"vault": "notes", "folder": "Ideas", "prefix": "PI-", "name": "idea", "content": "# {{id}}\n"}))["id"].as_str().unwrap().to_string();
    assert_eq!(id(&mut a), "PI-001");
    assert_eq!(id(&mut a), "PI-002");
    assert_eq!(id(&mut b), "PI-001", "beta's ids do not follow alpha's");

    // The decision queue.
    let thread = call(
        &mut a,
        "open_discussion",
        json!({"vault": "notes", "topic": "Compactor schedule", "subjects": [{"vault": "notes", "path": "Ideas/seed.md"}], "participants": ["bob", "alice"],
               "opening_post": {"author": "bob", "kind": "claim", "body": "Nightly at two is too late for the compactor."}}),
    )["thread"]
        .as_str()
        .unwrap()
        .to_string();
    call(&mut a, "post", json!({"thread": thread, "author": "alice", "kind": "challenge", "re": 1, "body": "Two is fine; the load is lowest then."}));
    call(
        &mut a,
        "mark_unresolved",
        json!({"thread": thread, "summary": "When the compactor runs.", "open_questions": ["Is two too late?"],
               "positions": [{"participant": "bob", "position": "Two is too late for the compactor.", "post": 1}, {"participant": "alice", "position": "Two is fine; the load is lowest.", "post": 2}],
               "settle_by": "A week of load samples."}),
    );
    assert_eq!(call(&mut a, "decision_queue", json!({}))["queue"].as_array().unwrap().len(), 1);
    assert!(call(&mut b, "decision_queue", json!({}))["queue"].as_array().unwrap().is_empty(), "beta's queue holds alpha's decision");
}

#[test]
fn the_user_layer_never_overwrites_a_projects_own_memory() {
    let two = Two::new("proj-collide");
    // beta has its own memory under the name a user-layer memory uses.
    let bm = two.root().join("beta").join("memory");
    memory_file(&bm, "kettle-descaling", "beta's kettle", "Beta's kettle is descaled weekly.", "");
    write(&bm.join("MEMORY.md"), "- [beta-flux](beta-flux.md) - beta private fact\n- [Kettle](kettle-descaling.md) - beta's kettle\n");
    let mut b = server_for(&two.resolve("beta").config);
    let k = text(&mut b, "recall", json!({"key": "kettle-descaling"}));
    assert!(k.contains("weekly") && !k.contains("citric"), "{k}");
}

fn text_err(s: &mut Server, tool: &str, args: Value) -> String {
    let mut a = args;
    if matches!(tool, "remember" | "supersede") && a.get("reason").is_none() {
        a["reason"] = json!("projects test: a synthetic write");
    }
    match s.answer(tool, &a) {
        Ok(x) => panic!("{tool} unexpectedly succeeded: {}", x.into_text()),
        Err(e) => e.to_string(),
    }
}

/// The `sha` line of `recall {key}`.
fn head_sha(s: &mut Server, key: &str) -> String {
    let r = text(s, "recall", json!({"key": key}));
    r.lines().find_map(|l| l.strip_prefix("sha ")).unwrap_or_else(|| panic!("no sha: {r}")).to_string()
}

#[test]
fn remember_with_scope_user_writes_the_shared_layer_every_project_reads() {
    let two = Two::new("proj-user-scope");
    let um = two.root().join("user-memory");
    let (ca, cb) = (two.resolve("alpha").config, two.resolve("beta").config);
    let mut a = server_for(&ca);

    // Written from alpha into the user layer's folder: the file and its index line.
    let out = text(
        &mut a,
        "remember",
        json!({"kind": "memory", "key": "grind-coarse", "title": "Coffee grind", "summary": "grind coarse for the press", "sub": "user", "scope": "user", "body": "Grind coarse for the French press."}),
    );
    assert!(out.contains("user layer"), "{out}");
    assert!(std::fs::read_to_string(um.join("grind-coarse.md")).unwrap().contains("Grind coarse for the French press."));
    let index = std::fs::read_to_string(um.join("MEMORY.md")).unwrap();
    assert_eq!(index, "- [Kettle descaling](kettle-descaling.md) - descale the kettle every month\n- [Coffee grind](grind-coarse.md) - grind coarse for the press\n", "the hand-written line is kept byte for byte");
    // A memory without scope stays in alpha.
    text(&mut a, "remember", json!({"kind": "memory", "key": "alpha-gauge", "body": "The alpha gauge reads in bar."}));
    assert!(!um.join("alpha-gauge.md").exists(), "a project memory never reaches the user layer");

    // beta reads it at its first rescan, and never alpha's own.
    let mut b = server_for(&cb);
    assert!(text(&mut b, "recall", json!({"key": "grind-coarse"})).contains("French press"));
    // Synced, it is scope user in beta too, as is the hand-written user layer.
    let scoped = text(&mut b, "recall", json!({"query": "grind coarse French press", "scope": "user"}));
    assert!(scoped.contains("grind-coarse"), "{scoped}");
    assert!(text(&mut b, "recall", json!({"query": "descale kettle citric acid", "scope": "user"})).contains("kettle-descaling"));
    assert!(text(&mut b, "recall", json!({"query": "beta flux capacitor noon", "scope": "user"})).contains("no records match"), "a project memory is not scope user");
    let e = text_err(&mut b, "recall", json!({"key": "alpha-gauge"}));
    assert!(e.contains("alpha-gauge"), "{e}");

    // A new version from alpha (scope inherited from the record): the file
    // follows, and beta's recall syncs it as the record's next version.
    let sha = head_sha(&mut a, "grind-coarse");
    text(
        &mut a,
        "remember",
        json!({"kind": "memory", "key": "grind-coarse", "title": "Coffee grind", "summary": "grind coarse for the press", "sub": "user", "expected_sha": sha, "body": "Grind coarse, thirty clicks, for the French press."}),
    );
    assert!(std::fs::read_to_string(um.join("grind-coarse.md")).unwrap().contains("thirty clicks"));
    assert!(text(&mut b, "recall", json!({"key": "grind-coarse"})).contains("thirty clicks"));
    let hist = text(&mut b, "recall", json!({"key": "grind-coarse", "history": true}));
    assert_eq!(hist.matches("== grind-coarse v").count(), 2, "beta keeps both versions: {hist}");

    // Superseded from beta this time: a new key in the user layer, in the old
    // one's place in the index; the old file is marked superseded.
    text(
        &mut b,
        "supersede",
        json!({"old": ["grind-coarse"], "key": "grind-medium", "title": "Coffee grind", "summary": "grind medium for the pour-over", "body": "Grind medium for the pour-over now.", "reason": "projects test: a synthetic write"}),
    );
    let index = std::fs::read_to_string(um.join("MEMORY.md")).unwrap();
    assert!(index.ends_with("- [Coffee grind](grind-medium.md) - grind medium for the pour-over\n") && !index.contains("grind-coarse.md"), "{index}");
    assert!(std::fs::read_to_string(um.join("grind-coarse.md")).unwrap().contains("superseded"), "the old file is marked superseded");
    let old_in_b = text(&mut b, "recall", json!({"key": "grind-coarse"}));
    assert!(old_in_b.contains("superseded"), "{old_in_b}");
    assert!(text(&mut a, "recall", json!({"key": "grind-medium"})).contains("pour-over"), "alpha reads beta's write");
    let old_in_a = text(&mut a, "recall", json!({"key": "grind-coarse", "history": true}));
    assert!(old_in_a.matches("== grind-coarse v").count() >= 2, "alpha keeps the history: {old_in_a}");

    // Only memories go to the user layer, and only where one is configured.
    assert!(text_err(&mut a, "remember", json!({"kind": "rule", "scope": "user", "body": "- a rule"})).contains("for memories"));
    let t = Temp::new("proj-user-none");
    write(&t.path().join("atlas.json"), r#"{"projects_dir": "state", "projects": [{"name": "solo", "roots": ["solo"]}], "watch": false}"#);
    std::fs::create_dir_all(t.path().join("solo")).unwrap();
    let c = Config::load(&t.path().join("atlas.json")).unwrap();
    let mut s = server_for(&project::resolve(&c, &t.path().join("solo"), None).unwrap().config);
    assert!(text_err(&mut s, "remember", json!({"kind": "memory", "key": "x", "scope": "user", "body": "x"})).contains("none is configured"));
}

fn open_unresolved(s: &mut Server, topic: &str, claim: &str) {
    let thread = call(
        s,
        "open_discussion",
        json!({"vault": "notes", "topic": topic, "subjects": [{"vault": "notes", "path": "Ideas/seed.md"}], "participants": ["bob", "alice"],
               "opening_post": {"author": "bob", "kind": "claim", "body": claim}}),
    )["thread"]
        .as_str()
        .unwrap()
        .to_string();
    call(
        s,
        "mark_unresolved",
        json!({"thread": thread, "summary": topic, "open_questions": ["Which way?"],
               "positions": [{"participant": "bob", "position": claim, "post": 1}], "settle_by": "A week of samples."}),
    );
}

#[test]
fn journal_status_and_decision_queue_filter_by_a_repo_and_its_neighbours() {
    let two = Two::new("proj-repo-filter");
    let (ca, cb) = (two.resolve("alpha").config, two.resolve("beta").config);
    let (mut a, mut b) = (server_for(&ca), server_for(&cb));
    let e = text_err(&mut a, "journal_status", json!({"repo": "ledger"}));
    assert!(e.contains("no project with repos"), "{e}");
    let repo = |n: &str, t: &[&str]| project::Repo { name: n.into(), path: None, remote: None, role: String::new(), talks_to: t.iter().map(|s| s.to_string()).collect() };
    let g = project::Graph { name: "alpha".into(), roots: vec![], repos: vec![repo("billing", &["ledger"]), repo("ledger", &[]), repo("gateway", &["auth"]), repo("auth", &[])], unmapped: vec![] };
    for (s, c) in [(&mut a, &ca), (&mut b, &cb)] {
        let mut st = settings_for(c);
        st.graph = Some(Arc::new(g.clone()));
        s.set_memory(st);
    }
    for (lane, t) in [("TRACK-1", "posting batches to the ledger"), ("TRACK-2", "billing retries after a failed charge"), ("TRACK-3", "gateway rate limits"), ("TRACK-4", "auth token rotation")] {
        text(&mut a, "journal", json!({"lane": lane, "entry": "note", "text": t}));
    }
    // ledger covers itself and billing, which talks to it.
    let js = text(&mut a, "journal_status", json!({"repo": "ledger"}));
    assert!(js.contains("TRACK-1") && js.contains("TRACK-2") && !js.contains("TRACK-3") && !js.contains("TRACK-4"), "{js}");
    let js = text(&mut a, "journal_status", json!({"repo": "Auth"}));
    assert!(js.contains("TRACK-3") && js.contains("TRACK-4") && !js.contains("TRACK-1") && !js.contains("TRACK-2"), "{js}");
    let all = text(&mut a, "journal_status", json!({}));
    assert_eq!(all.matches("TRACK-").count(), 4, "{all}");
    let e = text_err(&mut a, "journal_status", json!({"repo": "nope"}));
    assert!(e.contains("billing, ledger, gateway, auth"), "{e}");
    assert!(!text(&mut b, "journal_status", json!({"repo": "ledger"})).contains("TRACK-"), "the filter never reaches the other project's lanes");

    open_unresolved(&mut a, "Ledger posting cadence", "Post to the ledger hourly.");
    open_unresolved(&mut a, "Gateway rate limits", "Raise the gateway limit to a thousand.");
    let topics = |s: &mut Server, args: Value| -> Vec<String> { call(s, "decision_queue", args)["queue"].as_array().unwrap().iter().map(|t| t["topic"].as_str().unwrap().to_string()).collect() };
    assert_eq!(topics(&mut a, json!({"repo": "billing"})), vec!["Ledger posting cadence"]);
    assert_eq!(topics(&mut a, json!({"repo": "auth"})), vec!["Gateway rate limits"]);
    assert_eq!(topics(&mut a, json!({})).len(), 2);
    assert!(topics(&mut b, json!({"repo": "ledger"})).is_empty());
}

#[test]
fn the_offer_state_and_routing_switch_are_user_level_and_migrate_once() {
    let two = Two::new("proj-user-state");
    let r = two.root();
    let user = r.join("state");
    // State the per-project build left: alpha asked about a folder, beta
    // reported an unmapped repo and had routing off.
    write(&user.join("alpha").join(cs::OFFER_STATE), "{\"offered\": [\"one\"], \"projects\": [\"/w/acme\"]}\n");
    write(&user.join("beta").join(cs::OFFER_STATE), "{\"offered\": [\"one\", \"two\"], \"unmapped\": [\"beta:reports\"]}\n");
    write(&user.join("beta").join(atlas::hook_rewrite::OFF_FLAG), "off\n");
    let c = two.config();
    assert_eq!(project::user_state_dir(&c), Some(user.clone()));
    assert!(project::migrate_user_state(&c).unwrap(), "the first run moves the state");
    let merged: Value = json::parse(&std::fs::read_to_string(user.join(cs::OFFER_STATE)).unwrap()).unwrap();
    assert_eq!(merged["offered"], json!(["one", "two"]));
    assert_eq!(merged["projects"], json!(["/w/acme"]));
    assert_eq!(merged["unmapped"], json!(["beta:reports"]));
    assert!(user.join(atlas::hook_rewrite::OFF_FLAG).exists(), "routing off in any project is off for all");
    let before = std::fs::read_to_string(user.join(cs::OFFER_STATE)).unwrap();
    assert!(!project::migrate_user_state(&c).unwrap(), "once");
    assert_eq!(std::fs::read_to_string(user.join(cs::OFFER_STATE)).unwrap(), before);

    // Every project reads and writes the one state and the one switch.
    let (sa, sb) = (settings_for(&two.resolve("alpha").config), settings_for(&two.resolve("beta").config));
    assert_ne!(sa.index_db, sb.index_db);
    assert_eq!((sa.user_state.clone(), sb.user_state.clone()), (Some(user.clone()), Some(user.clone())));
    for s in [&sa, &sb] {
        assert!(!atlas::hook_rewrite::Router::new(s).routing_on());
        let p = Paths::detect_in(None, &s.index_db, s.user_state.as_deref()).unwrap();
        assert_eq!(p.state, Some(user.join(cs::OFFER_STATE)));
    }
    let p = paths(r, &r.join("atlas.json"));
    let (ok, out) = settings_cmd(&p, &["set", "routing", "on"]);
    assert!(ok, "{out}");
    assert!(atlas::hook_rewrite::Router::new(&sa).routing_on() && atlas::hook_rewrite::Router::new(&sb).routing_on(), "one switch turns routing on in both");

    // A folder asked about from alpha is not asked about again from beta,
    // and the settings offer keeps the folders it does not own.
    let acme = r.join("acme");
    repo(&acme.join("billing"), "git@git.example:acme/billing.git");
    repo(&acme.join("auth"), "git@git.example:acme/auth.git");
    let session_start = |s: &Settings| {
        let mut h = Hooks::open(s.clone()).unwrap();
        h.set_offer_paths(Some(Paths { db: Some(s.index_db.clone()), ..p.clone() }));
        h.set_offer_ctx(Some(OfferCtx::new(&c, &project::Graph::default())));
        h.handle(&HookEvent { event: "SessionStart".into(), cwd: acme.to_string_lossy().into_owned(), ..HookEvent::default() }).context
    };
    assert!(session_start(&sa).contains("holds 2 git repos"));
    assert!(session_start(&sb).contains("git repos"), "unanswered, so it returns in any project");
    let (ok, out) = settings_cmd(&p, &["add-project", "--no", &acme.to_string_lossy()]);
    assert!(ok, "{out}");
    assert!(!session_start(&sa).contains("git repos") && !session_start(&sb).contains("git repos"), "answered once for every project");
    let state: Value = json::parse(&std::fs::read_to_string(user.join(cs::OFFER_STATE)).unwrap()).unwrap();
    assert!(state["projects"].as_array().unwrap().len() == 2 && state["unmapped"] == json!(["beta:reports"]), "{state}");
}

#[test]
fn each_project_has_its_own_tree_and_shared_pins_reach_every_root() {
    let two = Two::new("proj-tree");
    let r = two.root();
    // One user-level tree for every project; a shared pin in the user layer.
    let cfg = std::fs::read_to_string(r.join("atlas.json")).unwrap().replace("\"watch\": false", "\"watch\": false,\n  \"memory\": {\"tree\": {\"pins\": [\"kettle-descaling\"]}}");
    write(&r.join("atlas.json"), &cfg);
    // Each project pins one of its own; alpha's scope: user memory is pinned too.
    memory_file(&r.join("alpha").join("memory"), "tabs-not-spaces", "indent with tabs everywhere", "Indent with tabs in every repository.", "scope: user\npinned: true\n");
    memory_file(&r.join("alpha").join("memory"), "alpha-widget", "alpha private fact", "The alpha widget compactor runs nightly at two.", "pinned: true\n");
    memory_file(&r.join("beta").join("memory"), "beta-flux", "beta private fact", "The beta flux capacitor drains at noon.", "pinned: true\n");
    for i in 0..6 {
        memory_file(&r.join("alpha").join("memory"), &format!("alpha-note-{i}"), &format!("alpha note {i} about the compactor"), "An alpha note.", "");
        memory_file(&r.join("beta").join("memory"), &format!("beta-note-{i}"), &format!("beta note {i} about the capacitor"), "A beta note.", "");
    }
    let c = two.config();
    assert!(c.memory.as_ref().unwrap().tree.as_ref().unwrap().export.is_empty(), "the user-level tree names no export");
    let mut lex = |t: &[String]| atlas::memory::tree::lexical_vectors(t);
    let mut roots = Vec::new();
    for p in ["alpha", "beta"] {
        let res = two.resolve(p);
        let m = res.config.memory.clone().unwrap();
        let spec = m.tree.clone().expect("each project gets the tree");
        assert_eq!(spec.export, "memory", "pointed at the project's own memory export");
        assert_eq!(spec.shared_pins, vec!["kettle-descaling".to_string()]);
        assert!(spec.pins.is_empty(), "user-level pins are shared, not the project's");
        let dir = r.join(p).join("memory");
        let (churn, _) = atlas::memory::tree::rebuild(&dir, "MEMORY.md", &spec, &mut lex, true).unwrap();
        assert!(churn.changed());
        let (again, _) = atlas::memory::tree::rebuild(&dir, "MEMORY.md", &spec, &mut lex, true).unwrap();
        assert!(!again.changed(), "{p}: a second rebuild writes nothing: {}", again.line());
        assert!(dir.join(&spec.hub_dir).is_dir(), "{p}: hubs in its own folder");
        let nav = atlas::memory::tree::check(&dir, "MEMORY.md", &spec).unwrap();
        assert!(nav.problems.is_empty(), "{p}: {:?}", nav.problems);
        assert_eq!(nav.reads[4], 0, "{p}: every memory within 3 reads");
        assert!(atlas::memory::tree::pin_drift(&dir, &spec).is_empty());
        roots.push(std::fs::read_to_string(dir.join("MEMORY.md")).unwrap());
    }
    assert!(!r.join("user-memory").join("index").exists(), "the user layer has no tree");
    let (ra, rb) = (&roots[0], &roots[1]);
    let shared = |root: &str| root.split("## Pinned for every project").nth(1).unwrap_or("").to_string();
    assert!(shared(ra).contains("* kettle-descaling:") && shared(rb).contains("* kettle-descaling:"), "a shared pin is in every root:\n{ra}\n{rb}");
    assert!(shared(rb).contains("* tabs-not-spaces:") && !shared(ra).contains("tabs-not-spaces"), "alpha's scope: user pin is shared with beta, and alpha's own in alpha:\n{rb}");
    assert!(ra.contains("alpha-widget") && !rb.contains("alpha-widget"), "a project pin stays in its project");
    assert!(rb.contains("beta-flux") && !ra.contains("beta-flux"));
    // The sync reads the tree's lines: recall still answers in both, with no bleed.
    let (mut a, mut b) = (server_for(&two.resolve("alpha").config), server_for(&two.resolve("beta").config));
    assert!(text(&mut a, "recall", json!({"key": "alpha-note-3"})).contains("An alpha note."));
    assert!(text_err(&mut b, "recall", json!({"key": "alpha-note-3"})).contains("alpha-note-3"));
    for s in [&mut a, &mut b] {
        assert!(text(s, "recall", json!({"key": "kettle-descaling"})).contains("citric acid"));
    }
}

/// A scratch home for `atlas settings`.
fn paths(root: &Path, config: &Path) -> Paths {
    std::fs::create_dir_all(root.join("claude")).unwrap();
    Paths { claude_dir: root.join("claude"), atlas_config: Some(config.to_path_buf()), state: Some(root.join("state").join(cs::OFFER_STATE)), db: Some(root.join("state").join("atlas.sqlite")), user_state: Some(root.join("state")), project: None }
}

fn settings_cmd(p: &Paths, args: &[&str]) -> (bool, String) {
    let mut out = Vec::new();
    let mut input: &[u8] = b"";
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let code = cs::run(&args, p, &mut Io { out: &mut out, input: &mut input, interactive: false, version: None, dry_run: false });
    match code {
        Ok(c) => (c == std::process::ExitCode::SUCCESS, String::from_utf8(out).unwrap()),
        Err(e) => (false, e),
    }
}

#[test]
fn migration_moves_a_setup_into_one_project_once_and_undoes() {
    let t = Temp::new("proj-migrate");
    let r = t.path();
    write(&r.join("work").join("notes").join("a.md"), "# A\n");
    write(&r.join("work").join("docs").join("b.md"), "# B\n");
    let cfg = r.join("atlas.json");
    let original = r#"{
  "db": "state/atlas.sqlite",
  "vaults": [{"name": "notes", "path": "work/notes"}],
  "sources": [{"name": "docs", "root": "work/docs", "include": ["*.md"]}],
  "memory": {"store": "state/memory.sqlite"},
  "update": {"enabled": false}
}
"#;
    write(&cfg, original);
    let before = Config::load(&cfg).unwrap();
    let p = paths(r, &cfg);
    let (ok, out) = settings_cmd(&p, &["migrate-projects", "alpha"]);
    assert!(ok, "{out}");
    let migrated = std::fs::read_to_string(&cfg).unwrap();
    let c = Config::load(&cfg).unwrap();
    assert!(c.user.scoped);
    assert_eq!(c.user.default_project.as_deref(), Some("alpha"));
    assert_eq!(c.update.as_ref().and_then(|u| u.enabled), Some(false), "user-level keys stay at the top");
    // Resolved anywhere, it is the same setup: same database, store, vaults, sources.
    for cwd in [r.join("work").join("docs"), std::env::temp_dir()] {
        let e = project::resolve(&c, &cwd, None).unwrap();
        assert_eq!(e.active.as_ref().map(|a| a.spec.name.as_str()), Some("alpha"));
        assert_eq!(e.config.db, before.db);
        assert_eq!(e.config.memory.as_ref().and_then(|m| m.store.clone()), before.memory.as_ref().and_then(|m| m.store.clone()));
        assert_eq!(e.config.vaults.len(), 1);
        assert_eq!(e.config.sources.len(), 1);
    }
    assert_eq!(project::resolve(&c, &r.join("work").join("docs"), None).unwrap().active.unwrap().why, Why::Root, "the root is the folder holding the old roots");
    // Idempotent.
    let (ok, out) = settings_cmd(&p, &["migrate-projects", "alpha"]);
    assert!(ok && out.contains("nothing to change"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), migrated);
    // Undo restores the file byte for byte.
    let (ok, out) = settings_cmd(&p, &["undo", "--yes"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
}

/// A git repository with an `origin` remote, as `git clone` leaves one (no
/// git is run).
fn repo(dir: &Path, remote: &str) {
    write(&dir.join(".git").join("config"), &format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {remote}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"));
    write(&dir.join("README.md"), &format!("# {}\n", dir.file_name().unwrap().to_string_lossy()));
}

const BILLING_PROTO: &str = r#"syntax = "proto3";
package acme.billing.v1;
import "acme/auth/v1/auth.proto";

message Invoice {
  string id = 1;
  acme.auth.v1.Principal owner = 2;
}
message GetInvoiceRequest { string id = 1; }
service InvoiceService {
  rpc GetInvoice(GetInvoiceRequest) returns (Invoice);
}
"#;

const AUTH_PROTO: &str = r#"syntax = "proto3";
package acme.auth.v1;
message Principal { string subject = 1; }
service AuthService {
  rpc Verify(Principal) returns (Principal);
}
"#;

const GATEWAY_GO: &str = "package main\n\nfunc handle(ctx context.Context) {\n\tinv, err := billingClient.GetInvoice(ctx, &billingpb.GetInvoiceRequest{Id: id})\n\t_, _ = authClient.Verify(ctx, p)\n}\n";

#[test]
fn a_root_of_repos_is_discovered_offered_written_routed_and_indexed() {
    let t = Temp::new("proj-multi");
    let r = t.path();
    let acme = r.join("acme");
    repo(&acme.join("billing"), "git@git.example:acme/billing.git");
    repo(&acme.join("auth"), "https://git.example/acme/auth.git");
    repo(&acme.join("gateway"), "ssh://git@git.example:22/acme/gateway");
    write(&acme.join("billing").join("proto").join("billing.proto"), BILLING_PROTO);
    write(&acme.join("auth").join("proto").join("acme").join("auth").join("v1").join("auth.proto"), AUTH_PROTO);
    write(&acme.join("gateway").join("cmd").join("main.go"), GATEWAY_GO);
    write(&acme.join("notes.txt"), "not a repo\n");
    // A fourth repository checked out somewhere else on this machine.
    let ledger = r.join("checkouts").join("ledger-src");
    repo(&ledger, "https://git.example/acme/ledger.git");
    let cfg = r.join("atlas.json");
    let original = r#"{"projects_dir": "state", "watch": false,
            "user": {"memory": "user-memory"},
            "projects": [{"name": "other", "roots": ["elsewhere"]}],
            "checkouts": {"https://git.example/acme/ledger": "checkouts/ledger-src"}}"#;
    write(&cfg, original);
    write(&r.join("user-memory").join("MEMORY.md"), "");
    let c = Config::load(&cfg).unwrap();

    // Discovery: one level down, a .git folder or file; remotes normalised.
    let kids = project::child_repos(&acme);
    let names: Vec<&str> = kids.iter().map(|k| k.0.as_str()).collect();
    assert_eq!(names, vec!["auth", "billing", "gateway"]);
    let remotes: Vec<String> = kids.iter().map(|k| project::normalize_remote(k.2.as_deref().unwrap())).collect();
    assert_eq!(remotes, vec!["git.example/acme/auth", "git.example/acme/billing", "git.example/acme/gateway"]);

    // Not in any project yet: the folder resolves to none.
    assert!(project::resolve(&c, &acme, None).unwrap().active.is_none());

    // The offer: one line, once per folder.
    let p = paths(r, &cfg);
    let session_start = |p: &Paths| {
        let mut h = Hooks::open(settings_for(&project::resolve(&c, &acme, None).unwrap().config)).unwrap();
        h.set_offer_paths(Some(p.clone()));
        h.set_offer_ctx(Some(OfferCtx::new(&c, &project::Graph::default())));
        h.handle(&HookEvent { event: "SessionStart".into(), cwd: acme.to_string_lossy().into_owned(), ..HookEvent::default() }).context
    };
    let line = session_start(&p);
    assert!(line.contains("holds 3 git repos (auth, billing, gateway)"), "{line}");
    assert!(line.contains("new project here") && line.contains("add to an existing project (other)") && line.contains("skip"), "{line}");
    assert!(line.contains("add-project --new"), "{line}");
    // The new-project answer folds in the vault, beside the configuration.
    let vault = r.join("vaults").join("acme-notes");
    let vault_shown = atlas::paths::canonical(r).join("vaults").join("acme-notes").to_string_lossy().replace('\\', "/");
    assert!(line.contains(&format!("and a vault for its notes at {vault_shown}")), "{line}");
    assert!(line.contains("--vault-at") && line.contains("--no-vault"), "{line}");
    assert!(session_start(&p).contains("git repos"), "unanswered, so it returns");

    // Yes: the project file, then undo removes it, then yes again.
    let acme_arg = acme.to_string_lossy().into_owned();
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", &acme_arg]);
    assert!(ok, "{out}");
    let pf = acme.join(project::PROJECT_FILE);
    let written: Value = json::parse(&std::fs::read_to_string(&pf).unwrap()).unwrap();
    assert_eq!(written["name"], json!("acme"));
    let listed: Vec<(String, String, String)> = written["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["name"].as_str().unwrap().into(), x["path"].as_str().unwrap().into(), x["remote"].as_str().unwrap().into()))
        .collect();
    assert_eq!(listed[1], ("billing".to_string(), "billing".to_string(), "git@git.example:acme/billing.git".to_string()));
    // Registered in the user configuration as {name, file}.
    let reg = Config::load(&cfg).unwrap();
    let entry = reg.user.projects.iter().find(|x| x.name == "acme").expect("acme is registered");
    // Recorded canonical (on macOS `$TMPDIR` is under /var, a link to
    // /private/var), and one folder with the file as given.
    assert_eq!(entry.file.as_deref(), Some(atlas::paths::canonical(&pf).as_path()));
    assert_eq!(entry.file.as_deref().map(project::path_key), Some(project::path_key(&pf)));
    // With its vault, in the project's own entry, never inside a repo.
    assert!(out.contains("vault acme-notes:") && vault.join(cs::VAULT_NOTE).is_file(), "{out}");
    let vaults: Vec<(&str, String)> = entry.body.vaults.iter().map(|v| (v.name.as_str(), project::path_key(&v.path))).collect();
    assert_eq!(vaults, vec![("acme-notes", project::path_key(&vault))]);
    assert!(reg.vaults.is_empty(), "not a top-level vault every project sees");
    let (ok, out) = settings_cmd(&p, &["undo", "--yes"]);
    assert!(ok && !pf.exists(), "undo removes the project file: {out}");
    assert!(!vault.exists(), "and the vault folder atlas made, still holding only its note: {out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original, "the same undo removes the registration");
    assert!(settings_cmd(&p, &["add-project", "--new", &acme_arg]).0);
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", &acme_arg]);
    assert!(ok && out.contains("registered already"), "{out}");

    // The team edits the committed file: roles, links, and repos listed by
    // remote only (one checked out elsewhere here, one not at all).
    write(
        &pf,
        r#"{
  "name": "acme",
  "repos": [
    {"name": "billing", "path": "billing", "remote": "git@git.example:acme/billing.git", "role": "invoices and payments", "talks_to": ["auth", "ledger"]},
    {"name": "auth", "path": "auth", "role": "identity and tokens"},
    {"name": "gateway", "path": "gateway", "role": "public API edge", "talks_to": ["billing", "auth"]},
    {"name": "ledger", "remote": "git@git.example:acme/ledger.git", "role": "double-entry books"},
    {"name": "reports", "remote": "git@git.example:acme/reports.git"}
  ],
  "memory_dir": "team-memory"
}
"#,
    );
    write(&acme.join("team-memory").join("MEMORY.md"), "- [Review in pairs](review-in-pairs.md) - every change gets two readers\n");
    write(
        &acme.join("team-memory").join("review-in-pairs.md"),
        "---\nname: review-in-pairs\ndescription: every change gets two readers\nscope: user\n---\n\nEvery change gets two readers before it merges.\n",
    );
    project::clear_cache();
    // Registered, the project owns its repos' checkouts elsewhere, and the
    // other projects read its memory folder for the user layer.
    let c2 = Config::load(&cfg).unwrap();
    let in_ledger = project::resolve(&c2, &ledger.join("src"), None).unwrap();
    assert_eq!(in_ledger.active.as_ref().map(|a| (a.spec.name.as_str(), a.why)), Some(("acme", Why::Root)), "a remote-mapped checkout elsewhere resolves to its project");
    let other = project::resolve(&c2, &r.join("elsewhere"), None).unwrap();
    assert_eq!(other.active.as_ref().map(|a| a.spec.name.as_str()), Some("other"));
    let from = &other.config.memory.as_ref().unwrap().user_scope_from;
    assert!(from.iter().any(|d| project::path_key(d) == project::path_key(&acme.join("team-memory"))), "{from:?}");
    let mut os = server_for(&other.config);
    assert!(text(&mut os, "recall", json!({"key": "review-in-pairs"})).contains("two readers"), "a scope: user memory in acme's folder reaches project other");
    project::clear_cache();
    let res = project::resolve(&c, &acme.join("gateway").join("cmd"), None).unwrap();
    assert_eq!(res.active.as_ref().map(|a| (a.spec.name.as_str(), a.why)), Some(("acme", Why::File)));
    let g = res.graph.clone();
    assert_eq!(g.repo("ledger").and_then(|x| x.path.clone()), Some(ledger.clone()), "a remote-only repo is found through the checkout map");
    assert_eq!(g.unmapped, vec!["reports (git@git.example:acme/reports.git)".to_string()]);
    assert_eq!(g.depends_on("gateway"), vec!["billing", "auth", "ledger"]);
    let e = res.config;
    for s in ["billing", "auth", "gateway", "ledger"] {
        assert!(e.sources.iter().any(|x| x.name == s), "source {s}");
        assert!(e.code.as_ref().unwrap().roots.iter().any(|x| x.name == format!("{s}-code")), "code root {s}");
    }
    assert_eq!(e.db.as_deref(), Some(r.join("state").join("acme").join("atlas.sqlite").as_path()));

    // The unmapped repo is reported once.
    let ctx = OfferCtx::new(&c, &g);
    let first = cs::unmapped_line(&p, &ctx).unwrap();
    assert!(first.contains("reports") && first.contains("checkouts"), "{first}");
    assert!(cs::unmapped_line(&p, &ctx).is_none());

    // Routing: the session's touched paths, by recency; the folder only as fallback.
    let mut settings = settings_for(&e);
    settings.graph = Some(Arc::new(g.clone()));
    let mut h = Hooks::open(settings).unwrap();
    h.set_offer_paths(None);
    let cwd = acme.join("gateway").to_string_lossy().into_owned();
    assert_eq!(h.active_repo("s", &cwd).as_deref(), Some("gateway"));
    let touch = |h: &mut Hooks, path: PathBuf| {
        let ev = HookEvent {
            event: "PostToolUse".into(),
            session_id: "s".into(),
            tool_name: "Read".into(),
            cwd: cwd.clone(),
            tool_input: json!({"file_path": path.to_string_lossy().into_owned()}),
            tool_response: json!({"type": "text"}),
            ..HookEvent::default()
        };
        h.handle(&ev);
    };
    for _ in 0..2 {
        touch(&mut h, acme.join("billing").join("proto").join("billing.proto"));
    }
    assert_eq!(h.active_repo("s", &cwd).as_deref(), Some("billing"));
    touch(&mut h, ledger.join("README.md"));
    touch(&mut h, ledger.join("README.md"));
    touch(&mut h, ledger.join("README.md"));
    assert_eq!(h.active_repo("s", &cwd).as_deref(), Some("ledger"), "a remote-mapped checkout routes too");
    assert_eq!(h.active_repo("other-session", &cwd).as_deref(), Some("gateway"));
    // A prompt naming a repo gets its line from the graph.
    let ans = h.handle(&HookEvent { event: "UserPromptSubmit".into(), session_id: "s".into(), prompt: "what does gateway depend on".into(), ..HookEvent::default() });
    assert!(ans.context.contains("repo gateway: public API edge; talks to billing, auth; through them ledger"), "{}", ans.context);

    // The .proto index across repos: definitions, type references, rpc call sites.
    let mut server = server_for(&e);
    let code = atlas::code::CodeSetup { defer_on_cargo: false, ..atlas::code::CodeSetup::from_spec(e.code.as_ref().unwrap(), e.db.as_ref().unwrap(), false) };
    let mut conn = rusqlite::Connection::open(e.db.as_ref().unwrap()).unwrap();
    let scan = atlas::code_index::RustSource::ScanOnly("test".into());
    let root = |n: &str| code.roots.iter().find(|x| x.name == n).unwrap().clone();
    let t0 = Instant::now();
    for n in ["gateway-code", "billing-code", "auth-code", "ledger-code"] {
        atlas::code_index::index_root(&code, &mut conn, &root(n), &scan).unwrap();
    }
    let fp: String = conn.query_row("SELECT fingerprint FROM code_roots WHERE root = 'gateway-code'", [], |x| x.get(0)).unwrap();
    assert!(fp.is_empty(), "a root indexed before an rpc appeared is marked for re-indexing");
    atlas::code_index::index_root(&code, &mut conn, &root("gateway-code"), &scan).unwrap();
    eprintln!("indexed 4 roots and re-indexed 1 in {:.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    server.set_code(atlas::tools::CodeState { setup: code.clone(), indexer: None });
    let who_defines = text(&mut server, "symbol", json!({"name": "Principal"}));
    assert!(who_defines.contains("auth.proto") && who_defines.contains("message"), "{who_defines}");
    assert!(who_defines.contains("billing.proto"), "a type used from another repo's proto is a reference: {who_defines}");
    let who_calls = text(&mut server, "symbol", json!({"name": "GetInvoice"}));
    assert!(who_calls.contains("rpc (GetInvoiceRequest) returns (Invoice)"), "{who_calls}");
    assert!(who_calls.contains("main.go"), "the gateway's call site: {who_calls}");
    let l = text(&mut server, "lookup", json!({"id": "GetInvoice"}));
    assert!(l.contains("in the code index") && l.contains("main.go"), "{l}");
    let auth_verify = text(&mut server, "symbol", json!({"name": "Verify"}));
    assert!(auth_verify.contains("main.go"), "{auth_verify}");
}

#[test]
fn resolution_costs_microseconds_and_is_cached() {
    let two = Two::new("proj-cost");
    let c = two.config();
    let cwd = two.root().join("beta").join("vault").join("Ideas");
    let t0 = Instant::now();
    let first = project::active(&c, &cwd, None).unwrap().unwrap();
    let cold = t0.elapsed();
    assert_eq!(first.spec.name, "beta");
    let n = 2000u32;
    let t1 = Instant::now();
    for _ in 0..n {
        project::active(&c, &cwd, None).unwrap();
    }
    let warm = t1.elapsed() / n;
    let t2 = Instant::now();
    for _ in 0..200 {
        project::resolve(&c, &cwd, None).unwrap();
    }
    let full = t2.elapsed() / 200;
    eprintln!("project resolution: first {:.1} us, cached {:.2} us, whole merged configuration {:.1} us", cold.as_secs_f64() * 1e6, warm.as_secs_f64() * 1e6, full.as_secs_f64() * 1e6);
    assert!(warm.as_micros() < 200, "cached resolution {warm:?}");
}

#[test]
fn recall_puts_the_active_repos_records_first_then_its_neighbours() {
    let t = Temp::new("proj-boost");
    let r = t.path();
    let repo_at = |n: &str| project::Repo { name: n.into(), path: Some(r.join(n)), role: String::new(), remote: None, talks_to: Vec::new() };
    let mut billing = repo_at("billing");
    billing.talks_to = vec!["ledger".into()];
    let g = project::Graph { name: "acme".into(), roots: vec![r.to_path_buf()], repos: vec![billing, repo_at("ledger"), repo_at("reports")], unmapped: Vec::new() };
    let mut s = Settings::from_config(None, &r.join("state").join("atlas.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.recall.fts_floor = 0.0;
    s.recall.hybrid_fts_floor = 0.0;
    s.graph = Some(Arc::new(g));
    // A tree branch can name the repo its memories are about.
    let mut tree = atlas::memory::tree::TreeSpec::new("memory");
    tree.branches.push(atlas::memory::tree::BranchSpec { name: "payments".into(), title: "Payments".into(), hook: String::new(), words: Vec::new(), types: Vec::new(), repo: Some("ledger".into()) });
    s.tree = Some(tree);
    {
        let mut store = atlas::memory::MemoryStore::open(&s.store).unwrap();
        let mut w = store.writer().unwrap();
        // Tagged by `repo:`, by `branch:` through the tree's map, and one
        // that only mentions the active repo in its text: no tag, no boost.
        for (key, repo, tag) in [
            ("retry-reports", "reports", "repo: reports"),
            ("retry-plain", "ledger", ""),
            ("retry-billing", "billing", "repo: Billing"),
            ("retry-ledger", "ledger", "branch: payments"),
        ] {
            let d = atlas::memory::Draft {
                title: format!("- [Retry policy]({key}.md) - invoice retry policy in {repo}"),
                meta: format!("---\nname: {key}\n{tag}\n---\n"),
                body: format!("\nThe invoice retry policy in {repo} backs off exponentially after a failed posting.\n"),
                reason: "seeding the boost test".into(),
                author: "t".into(),
                client: "t".into(),
                ..atlas::memory::Draft::default()
            };
            w.create(key, "memory", "feedback", "", &d, None).unwrap();
        }
        w.commit().unwrap();
    }
    let mut h = Hooks::open(s).unwrap();
    h.set_offer_paths(None);
    for _ in 0..3 {
        h.handle(&HookEvent {
            event: "PreToolUse".into(),
            session_id: "s".into(),
            tool_name: "Read".into(),
            tool_input: json!({"file_path": r.join("ledger").join("src").join("post.go").to_string_lossy().into_owned()}),
            ..HookEvent::default()
        });
    }
    let ans = h.handle(&HookEvent { event: "UserPromptSubmit".into(), session_id: "s".into(), prompt: "how does the invoice retry policy back off after a failed posting".into(), ..HookEvent::default() });
    let order: Vec<usize> = ["retry-ledger", "retry-billing", "retry-reports", "retry-plain"].iter().map(|k| ans.context.find(k).unwrap_or_else(|| panic!("{k} missing: {}", ans.context))).collect();
    assert!(order[0] < order[1] && order[1] < order[2] && order[1] < order[3], "the active repo's tag, then its neighbour's, then the rest (untagged included): {}", ans.context);
    assert_eq!(atlas::hooks::repo_tag("---\nname: x\n---\n", None), None);
}

#[test]
fn moving_a_memory_to_another_repo_is_a_new_version_not_a_new_record() {
    let two = Two::new("proj-retag");
    let m = two.root().join("alpha").join("memory");
    memory_file(&m, "engine-step", "the engine steps at a fixed rate", "The engine steps physics at a fixed rate.", "repo: game\nbranch: game\n");
    write(&m.join("MEMORY.md"), "- [alpha-widget](alpha-widget.md) - alpha private fact\n- [Engine step](engine-step.md) - the engine steps at a fixed rate\n");
    let ca = two.resolve("alpha").config;
    let mut a = server_for(&ca);
    let store_path = settings_for(&ca).store;
    let tag = || atlas::hooks::repo_tag(&atlas::memory::MemoryStore::open(&store_path).unwrap().head("engine-step").unwrap().unwrap().meta, None);
    let count = |s: &mut Server| text(s, "memory_log", json!({"limit": 2000})).lines().count();
    assert!(text(&mut a, "recall", json!({"key": "engine-step"})).contains("fixed rate"));
    assert_eq!(tag().as_deref(), Some("game"));
    let before = count(&mut a);
    // The engine moves to a new repo: only its tag and branch change.
    memory_file(&m, "engine-step", "the engine steps at a fixed rate", "The engine steps physics at a fixed rate.", "repo: engine\nbranch: engine\n");
    text(&mut a, "recall", json!({"key": "engine-step"}));
    assert_eq!(tag().as_deref(), Some("engine"));
    let hist = text(&mut a, "recall", json!({"key": "engine-step", "history": true}));
    assert_eq!(hist.matches("== engine-step v").count(), 2, "one record, a second version: {hist}");
    let log = text(&mut a, "memory_log", json!({"limit": 2000}));
    assert_eq!(log.lines().count(), before + 1, "nothing but the new version was written: {log}");
}

#[test]
fn a_workspace_root_that_is_a_checkout_is_no_member() {
    let t = Temp::new("proj-ws-root");
    let r = t.path();
    let ws = r.join("ws");
    // The workspace folder is itself a checkout of an unrelated repo.
    repo(&ws, "git@git.example:someone/dotfiles.git");
    for n in ["engine", "game", "tools"] {
        repo(&ws.join(n), &format!("git@git.example:acme/{n}.git"));
    }
    let cfg = r.join("atlas.json");
    write(&cfg, r#"{"projects_dir": "state", "watch": false, "projects": [{"name": "other", "roots": ["elsewhere"]}]}"#);
    let c = Config::load(&cfg).unwrap();
    let p = paths(r, &cfg);
    let mut h = Hooks::open(settings_for(&project::resolve(&c, &ws, None).unwrap().config)).unwrap();
    h.set_offer_paths(Some(p.clone()));
    h.set_offer_ctx(Some(OfferCtx::new(&c, &project::Graph::default())));
    let line = h.handle(&HookEvent { event: "SessionStart".into(), cwd: ws.to_string_lossy().into_owned(), ..HookEvent::default() }).context;
    assert!(line.contains("holds 3 git repos (engine, game, tools)"), "{line}");
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", &ws.to_string_lossy()]);
    assert!(ok, "{out}");
    let file = std::fs::read_to_string(ws.join(project::PROJECT_FILE)).unwrap();
    assert!(!file.contains("dotfiles"), "the root's own remote is not pulled in: {file}");
    project::clear_cache();
    let c2 = Config::load(&cfg).unwrap();
    let res = project::resolve(&c2, &ws, None).unwrap();
    assert_eq!(res.active.as_ref().map(|a| a.spec.name.as_str()), Some("ws"));
    let names: Vec<&str> = res.graph.repos.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["engine", "game", "tools"]);
    assert!(!res.config.sources.iter().any(|s| project::path_key(&s.root) == project::path_key(&ws)), "the root checkout is no source");
    assert!(res.graph.repo_of(&ws.join("README.md").to_string_lossy()).is_none(), "a file of the root checkout is in no repo");
    // One child repo: still only the child.
    let one = r.join("one");
    repo(&one, "git@git.example:someone/notes.git");
    repo(&one.join("lib"), "git@git.example:acme/lib.git");
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", &one.to_string_lossy()]);
    assert!(ok, "{out}");
    let f1 = std::fs::read_to_string(one.join(project::PROJECT_FILE)).unwrap();
    assert!(f1.contains("acme/lib") && !f1.contains("someone/notes"), "{f1}");
}

#[test]
fn a_new_project_takes_its_vault_where_told_or_none() {
    let t = Temp::new("proj-new-vault");
    let r = t.path();
    let cfg = r.join("atlas.json");
    let original = r#"{"projects_dir": "state", "watch": false, "projects": [{"name": "other", "roots": ["elsewhere"]}]}"#;
    write(&cfg, original);
    let p = paths(r, &cfg);
    // --name names the vault too; --vault-at puts it in a folder the user
    // named, which was there and is kept on undo.
    let ws = r.join("ws");
    repo(&ws.join("engine"), "git@git.example:acme/engine.git");
    let mine = r.join("my-notes");
    write(&mine.join("Idea.md"), "# Idea\n");
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", "--name", "studio", "--vault-at", &mine.to_string_lossy(), &ws.to_string_lossy()]);
    assert!(ok && out.contains("vault studio-notes:") && out.contains("an existing folder"), "{out}");
    let c = Config::load(&cfg).unwrap();
    let e = c.user.projects.iter().find(|x| x.name == "studio").unwrap();
    assert_eq!(e.body.vaults.iter().map(|v| project::path_key(&v.path)).collect::<Vec<_>>(), vec![project::path_key(&mine)]);
    let (ok, out) = settings_cmd(&p, &["undo", "--yes"]);
    assert!(ok && out.contains("is kept") && mine.join("Idea.md").is_file() && !ws.join(project::PROJECT_FILE).exists(), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
    // The project folder itself is no vault: refused before anything is written.
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", "--vault-at", &ws.to_string_lossy(), &ws.to_string_lossy()]);
    assert!(!ok && out.contains("is the project folder") && !ws.join(project::PROJECT_FILE).exists(), "{out}");
    // --no-vault: the project alone.
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", "--no-vault", &ws.to_string_lossy()]);
    assert!(ok && !out.contains("vault "), "{out}");
    let c = Config::load(&cfg).unwrap();
    assert!(c.user.projects.iter().find(|x| x.name == "ws").unwrap().body.vaults.is_empty());
    assert!(!r.join("vaults").exists());
}

/// A directory link `link` to `target`: a symlink, else on Windows (where a
/// symlink needs a privilege or developer mode) a junction. `Err` says why
/// neither could be made.
fn dir_link(target: &Path, link: &Path) -> Result<&'static str, String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map(|_| "symlink").map_err(|e| e.to_string())
    }
    #[cfg(windows)]
    {
        let sym = match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => return Ok("symlink"),
            Err(e) => e.to_string(),
        };
        let out = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target).output().map_err(|e| format!("symlink: {sym}; junction: {e}"))?;
        if out.status.success() && link.is_dir() {
            Ok("junction")
        } else {
            Err(format!("symlink: {sym}; junction: {}", String::from_utf8_lossy(&out.stderr).trim()))
        }
    }
}

/// The macOS shape everywhere: `$TMPDIR` is under /var, a link to
/// /private/var, so one folder arrives in two spellings (the configured or
/// event path, and the canonical one). Here a link stands in for /var.
#[test]
fn a_project_reached_through_a_link_resolves_routes_and_is_offered_once() {
    let t = Temp::new("proj-link");
    let r = t.path();
    let real = r.join("real");
    let acme = real.join("acme");
    repo(&acme.join("billing"), "git@git.example:acme/billing.git");
    repo(&acme.join("auth"), "git@git.example:acme/auth.git");
    std::fs::create_dir_all(real.join("beta").join("docs")).unwrap();
    let link = r.join("link");
    match dir_link(&real, &link) {
        Ok(kind) => eprintln!("folders reached through a {kind}"),
        Err(why) => {
            eprintln!("SKIPPED: no directory symlink or junction can be made here ({why})");
            return;
        }
    }
    let via = link.join("acme");
    // Project beta's root is configured through the link (the raw $TMPDIR
    // form); a working directory arrives canonical (the getcwd form).
    let cfg = r.join("atlas.json");
    write(&cfg, r#"{"projects_dir": "state", "watch": false, "projects": [{"name": "other", "roots": ["elsewhere"]}, {"name": "beta", "roots": ["link/beta"]}]}"#);
    let c = Config::load(&cfg).unwrap();
    project::clear_cache();
    let beta = project::resolve(&c, &real.join("beta").join("docs"), None).unwrap();
    assert_eq!(beta.active.as_ref().map(|a| (a.spec.name.as_str(), a.why)), Some(("beta", Why::Root)), "a canonical cwd is inside a root configured through a link");
    let beta = project::resolve(&c, &link.join("beta").join("docs").join("not-yet"), None).unwrap();
    assert_eq!(beta.active.as_ref().map(|a| a.spec.name.as_str()), Some("beta"), "and so is a cwd through the link, existing or not");
    assert!(project::resolve(&c, &via, None).unwrap().active.is_none(), "acme is no project yet");

    // The offer: answered once for the folder, whichever way it is reached.
    let p = paths(r, &cfg);
    let session_start = |cwd: &Path| {
        let mut h = Hooks::open(settings_for(&project::resolve(&c, cwd, None).unwrap().config)).unwrap();
        h.set_offer_paths(Some(p.clone()));
        h.set_offer_ctx(Some(OfferCtx::new(&c, &project::Graph::default())));
        h.handle(&HookEvent { event: "SessionStart".into(), cwd: cwd.to_string_lossy().into_owned(), ..HookEvent::default() }).context
    };
    let line = session_start(&via);
    assert!(line.contains("holds 2 git repos (auth, billing)"), "{line}");
    assert!(session_start(&acme).contains("git repos"), "unanswered, so the real path is asked too");
    let (ok, out) = settings_cmd(&p, &["add-project", "--no", &via.to_string_lossy()]);
    assert!(ok, "{out}");
    assert!(!session_start(&acme).contains("git repos"), "the same folder by its real path is not asked again");
    assert!(!session_start(&via).contains("git repos"), "nor through the link again");

    // Written through the link: one file, registered canonical, and the
    // spelling it was reached by is the same project file.
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", &via.to_string_lossy()]);
    assert!(ok, "{out}");
    let pf = acme.join(project::PROJECT_FILE);
    assert!(pf.is_file());
    let c2 = Config::load(&cfg).unwrap();
    let entry = c2.user.projects.iter().find(|x| x.name == "acme").expect("acme is registered");
    assert_eq!(entry.file.as_deref(), Some(atlas::paths::canonical(&pf).as_path()));
    assert_eq!(entry.file.as_deref().map(project::path_key), Some(project::path_key(&via.join(project::PROJECT_FILE))));
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", &acme.to_string_lossy()]);
    assert!(ok && out.contains("registered already"), "the real path is the registered project: {out}");

    // Resolution: a cwd through the link is inside the repo's checkout (a
    // root), not merely below the project file.
    project::clear_cache();
    let res = project::resolve(&c2, &via.join("billing").join("src"), None).unwrap();
    assert_eq!(res.active.as_ref().map(|a| (a.spec.name.as_str(), a.why)), Some(("acme", Why::Root)));
    let g = res.graph.clone();
    assert_eq!(g.repo_of(&via.join("auth").join("README.md").to_string_lossy()).map(|x| x.name.as_str()), Some("auth"));
    assert_eq!(g.repo_of(&acme.join("auth").join("new.rs").to_string_lossy()).map(|x| x.name.as_str()), Some("auth"));

    // Routing: touched paths through the link (as Claude Code sends them,
    // not canonical) route to their repo; the cwd through the link is the
    // fallback.
    let mut settings = settings_for(&res.config);
    settings.graph = Some(Arc::new(g));
    let mut h = Hooks::open(settings).unwrap();
    h.set_offer_paths(None);
    let cwd = via.join("billing").to_string_lossy().into_owned();
    assert_eq!(h.active_repo("s", &cwd).as_deref(), Some("billing"));
    for _ in 0..3 {
        let ev = HookEvent {
            event: "PostToolUse".into(),
            session_id: "s".into(),
            tool_name: "Read".into(),
            cwd: cwd.clone(),
            tool_input: json!({"file_path": via.join("auth").join("README.md").to_string_lossy().into_owned()}),
            tool_response: json!({"type": "text"}),
            ..HookEvent::default()
        };
        h.handle(&ev);
    }
    assert_eq!(h.active_repo("s", &cwd).as_deref(), Some("auth"), "a touch through the link routes to its repo");
    assert_eq!(h.active_repo("other", &acme.join("billing").to_string_lossy()).as_deref(), Some("billing"));
}

/// A repository whose folder is already a configured source or code root
/// reuses it; its Markdown source leaves out what other sources index; a
/// repository with no prior coverage is added as before; and recall still
/// puts the records of the repository holding the touched files first.
#[test]
fn a_repo_reuses_configured_roots_and_indexes_each_file_once() {
    let t = Temp::new("proj-dedup");
    let r = t.path();
    let alpha = r.join("alpha");
    repo(&alpha, "https://git.example/acme/alpha.git");
    write(&alpha.join("docs").join("guide.md"), "# Guide\n\nThe deployment guide for alpha.\n");
    write(&alpha.join("docs").join("deep").join("more.md"), "# More\n\nMore of the guide.\n");
    write(&alpha.join("notes").join("plan.md"), "# Plan\n\nThe alpha plan.\n");
    write(&alpha.join("INTENT.md"), "# Intent\n\n- Every rule is a line.\n");
    write(&alpha.join("src").join("lib.rs"), "pub fn alpha_entry() {}\n");
    let beta = r.join("beta");
    repo(&beta, "https://git.example/acme/beta.git");
    write(&beta.join("src").join("lib.rs"), "pub fn beta_entry() {}\n");
    let gamma = r.join("gamma");
    repo(&gamma, "https://git.example/acme/gamma.git");
    write(&gamma.join("guide.md"), "# Gamma guide\n\nText.\n");
    let cfg = r.join("atlas.json");
    write(
        &cfg,
        r#"{"projects_dir": "state", "watch": false,
            "projects": [{"name": "acme", "roots": ["."],
              "repos": [{"name": "alpha", "path": "alpha", "talks_to": ["beta"]}, {"name": "beta", "path": "beta"}, {"name": "gamma", "path": "gamma"}],
              "intent": "alpha/INTENT.md",
              "sources": [{"name": "docs", "root": "alpha/docs", "include": ["**/*.md"]}, {"name": "gamma-md", "root": "gamma", "include": ["*.md"]}],
              "code": {"roots": [{"name": "alpha-src", "root": "alpha"}]}}]}"#,
    );
    let c = Config::load(&cfg).unwrap();
    project::clear_cache();
    let res = project::resolve(&c, &alpha.join("src"), None).unwrap();
    let e = &res.config;
    let roots_at = |dir: &Path| -> Vec<String> {
        let k = project::path_key(dir);
        e.code.as_ref().unwrap().roots.iter().filter(|x| project::path_key(&x.root) == k).map(|x| x.name.clone()).collect()
    };
    let sources_at = |dir: &Path| -> Vec<String> {
        let k = project::path_key(dir);
        e.sources.iter().filter(|x| project::path_key(&x.root) == k).map(|x| x.name.clone()).collect()
    };
    // The configured code root is reused; gamma's configured source is too.
    assert_eq!(roots_at(&alpha), vec!["alpha-src"]);
    assert_eq!(sources_at(&gamma), vec!["gamma-md"]);
    assert_eq!(roots_at(&gamma), vec!["gamma-code"], "a source at the folder is no code root");
    // alpha's Markdown source leaves out the docs source's files and the intent file.
    let a = e.sources.iter().find(|x| x.name == "alpha").expect("alpha source");
    assert!(a.exclude.iter().any(|g| g == "docs/**/*.md"), "{:?}", a.exclude);
    assert!(a.exclude.iter().any(|g| g == "INTENT.md"), "{:?}", a.exclude);
    // beta, with nothing configured at its folder, is exactly as before.
    let b = e.sources.iter().find(|x| x.name == "beta").expect("beta source");
    assert_eq!(
        (b.root.as_path(), b.include.clone(), b.exclude.clone()),
        (beta.as_path(), vec!["**/*.md".to_string()], vec!["target/**".to_string(), "node_modules/**".into(), "vendor/**".into()])
    );
    assert_eq!(roots_at(&beta), vec!["beta-code"]);
    assert_eq!(sources_at(&beta), vec!["beta"]);

    // Indexed: every Markdown file once.
    let mut s = server_for(e);
    s.store_mut().rescan_sources(None).unwrap();
    let info = s.store_mut().list_sources().unwrap();
    let files = |n: &str| info.iter().find(|x| x.name == n).map(|x| x.files).unwrap_or_else(|| panic!("no source {n}"));
    assert_eq!(
        (files("alpha"), files("docs"), files("intent"), files("gamma-md"), files("beta")),
        (2, 2, 1, 2, 1),
        "README and plan; the guides; INTENT.md; gamma's two; beta's README"
    );
    let total: usize = info.iter().filter(|x| x.kind == "source").map(|x| x.files).sum();
    assert_eq!(total, 8, "eight Markdown files, each indexed once");

    // The repository of a file under a reused root is the one holding it,
    // so touching it makes that repository active and its records first.
    let g = res.graph.clone();
    let p = |dir: &Path, rel: &[&str]| rel.iter().fold(dir.to_path_buf(), |d, x| d.join(x)).to_string_lossy().into_owned();
    assert_eq!(g.repo_of(&p(&alpha, &["docs", "guide.md"])).map(|x| x.name.as_str()), Some("alpha"));
    assert_eq!(g.repo_of(&p(&alpha, &["src", "lib.rs"])).map(|x| x.name.as_str()), Some("alpha"));
    assert_eq!(g.repo_of(&p(&gamma, &["guide.md"])).map(|x| x.name.as_str()), Some("gamma"));
    let mut settings = settings_for(e);
    settings.graph = Some(Arc::new(g));
    {
        let mut store = atlas::memory::MemoryStore::open(&settings.store).unwrap();
        let mut w = store.writer().unwrap();
        for (key, repo) in [("rollout-gamma", "gamma"), ("rollout-alpha", "alpha")] {
            let d = atlas::memory::Draft {
                title: format!("- [Rollout]({key}.md) - staged rollout policy in {repo}"),
                meta: format!("---\nname: {key}\nrepo: {repo}\n---\n"),
                body: format!("\nThe staged rollout policy in {repo} waits a day between rings.\n"),
                reason: "seeding the dedup boost test".into(),
                author: "t".into(),
                client: "t".into(),
                ..atlas::memory::Draft::default()
            };
            w.create(key, "memory", "feedback", "", &d, None).unwrap();
        }
        w.commit().unwrap();
    }
    let mut h = Hooks::open(settings).unwrap();
    h.set_offer_paths(None);
    for (dir, rel) in [(&gamma, &["guide.md"][..]), (&alpha, &["docs", "guide.md"]), (&alpha, &["src", "lib.rs"]), (&alpha, &["docs", "deep", "more.md"])] {
        h.handle(&HookEvent { event: "PreToolUse".into(), session_id: "s".into(), tool_name: "Read".into(), tool_input: json!({"file_path": p(dir, rel)}), ..HookEvent::default() });
    }
    assert_eq!(h.active_repo("s", "").as_deref(), Some("alpha"));
    let ans = h.handle(&HookEvent { event: "UserPromptSubmit".into(), session_id: "s".into(), prompt: "how long does the staged rollout policy wait between rings".into(), ..HookEvent::default() });
    let ia = ans.context.find("rollout-alpha").unwrap_or_else(|| panic!("{}", ans.context));
    let ig = ans.context.find("rollout-gamma").unwrap_or_else(|| panic!("{}", ans.context));
    assert!(ia < ig, "the active repo's record first: {}", ans.context);
}

/// `--config PATH` before a subcommand word is that subcommand's
/// configuration: `atlas --config C settings migrate-projects NAME` moves C
/// into a project (it was read as the `settings on|off` switch).
#[test]
fn a_leading_config_reaches_the_settings_subcommand() {
    let t = Temp::new("proj-lead-cfg");
    let r = t.path();
    let cfg = r.join("cfg").join("atlas.json");
    write(&r.join("cfg").join("notes").join("a.md"), "# A\n");
    write(&cfg, r#"{"db": "state/atlas.sqlite", "vaults": [{"name": "notes", "path": "notes"}]}"#);
    let claude = r.join("claude");
    std::fs::create_dir_all(&claude).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
        .arg("--config")
        .arg(&cfg)
        .args(["settings", "migrate-projects", "alpha"])
        .env("CLAUDE_CONFIG_DIR", &claude)
        .env("ATLAS_HOME", r.join("home-atlas"))
        .env("HOME", r)
        .env("USERPROFILE", r)
        .env_remove("ATLAS_CONFIG")
        .env_remove("ATLAS_PROJECT")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    assert!(!text.contains("expected on or off"), "{text}");
    let c = Config::load(&cfg).unwrap();
    assert_eq!(c.user.projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["alpha"], "{text}");
}

/// A configuration with two projects, `alpha` and `beta` (each with a root
/// folder and a vault of its own), and no top-level vaults or sources.
fn two_projects(r: &Path) -> (PathBuf, String) {
    write(&r.join("alpha").join("a.md"), "# A\n");
    write(&r.join("beta").join("b.md"), "# B\n");
    write(&r.join("va").join("n.md"), "# NA\n");
    write(&r.join("vb").join("n.md"), "# NB\n");
    let cfg = r.join("atlas.json");
    let text = r#"{"projects_dir": "state", "watch": false,
  "projects": [
    {"name": "alpha", "roots": ["alpha"], "vaults": [{"name": "va", "path": "va"}]},
    {"name": "beta", "roots": ["beta"], "vaults": [{"name": "vb", "path": "vb"}]}
  ]
}
"#
    .to_string();
    write(&cfg, &text);
    (cfg, text)
}

/// #73: a repository folder over 32 characters gets a source name that
/// fits; every later run loads; and a name over the cap written by hand is
/// refused by the loader, naming the file to fix.
#[test]
fn a_long_repo_folder_never_bricks_the_configuration() {
    let t = Temp::new("proj-long-name");
    let r = t.path();
    let cfg = r.join("atlas.json");
    write(&cfg, r#"{"projects_dir": "state", "watch": false, "projects": [{"name": "other", "roots": ["elsewhere"]}]}"#);
    let p = paths(r, &cfg);
    let ws = r.join("ws");
    let long = "socket-account-config-manager-cli";
    repo(&ws.join(long), "git@git.example:acme/socket-account-config-manager-cli.git");
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", "--no-vault", &ws.to_string_lossy()]);
    assert!(ok, "{out}");
    let c = Config::load(&cfg).unwrap();
    let res = project::resolve(&c, &ws.join(long), None).unwrap();
    let s = res.config.sources.iter().find(|s| project::path_key(&s.root) == project::path_key(&ws.join(long))).unwrap();
    assert!(s.name.len() <= 32, "{}", s.name);
    atlas::store::AtlasSetup::from_config(&res.config).expect("the project's configuration runs");
    // A listed repo name up to 64 characters (as a project file may hold
    // one written before the cap) still runs: its source name is cut.
    let file = ws.join(project::PROJECT_FILE);
    write(&file, &format!(r#"{{"name": "ws", "repos": [{{"name": "{long}-and-then-some", "path": "{long}"}}]}}"#));
    project::clear_cache();
    let c = Config::load(&cfg).unwrap();
    let res = project::resolve(&c, &ws.join(long), None).unwrap();
    atlas::store::AtlasSetup::from_config(&res.config).expect("a long listed name runs");
    // A source name over the cap, by hand: the loader names the file.
    write(&file, &format!(r#"{{"name": "ws", "sources": [{{"name": "{long}", "root": ".", "include": ["*.md"]}}]}}"#));
    project::clear_cache();
    let e = Config::load(&cfg).unwrap_err().to_string();
    assert!(e.replace('\\', "/").contains("ws/atlas.project.json") && e.contains("sources[0].name") && e.contains(long), "{e}");
}

/// #74: in a two-project configuration add-project writes into the project
/// named (or the folder's own), never the top level that every project
/// reads, and leaves the other project as it was.
#[test]
fn add_project_writes_only_into_the_active_project() {
    let t = Temp::new("proj-add-active");
    let r = t.path();
    let (cfg, _) = two_projects(r);
    let client = r.join("client");
    write(&client.join("README.md"), "# Client\n");
    write(&client.join("src").join("lib.rs"), "pub fn f() {}\n");
    let mut p = paths(r, &cfg);
    p.project = Some("beta".into());
    let before = Config::load(&cfg).unwrap();
    let (ok, out) = settings_cmd(&p, &["add-project", &client.to_string_lossy()]);
    assert!(ok && out.contains("indexed source client") && out.contains("project beta"), "{out}");
    let c = Config::load(&cfg).unwrap();
    assert!(c.sources.is_empty() && c.vaults.is_empty() && c.code.is_none(), "nothing at the top level");
    let beta = c.user.projects.iter().find(|x| x.name == "beta").unwrap();
    assert_eq!(beta.body.sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["client"]);
    assert_eq!(beta.body.code.as_ref().unwrap().roots.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["client-code"]);
    let (a0, a1) = (before.user.projects.iter().find(|x| x.name == "alpha").unwrap(), c.user.projects.iter().find(|x| x.name == "alpha").unwrap());
    assert!(a1.body.sources.is_empty() && a1.body.code.is_none() && a1.body.vaults.len() == a0.body.vaults.len(), "alpha is untouched");
    // Alpha's resolved configuration does not read the client's files.
    project::clear_cache();
    let ra = project::resolve(&c, &r.join("alpha"), None).unwrap().config;
    assert!(!ra.sources.iter().any(|s| s.name == "client"), "{:?}", ra.sources.iter().map(|s| &s.name).collect::<Vec<_>>());
    // The folder is now covered: a second run changes nothing.
    let text = std::fs::read_to_string(&cfg).unwrap();
    let (ok, out) = settings_cmd(&p, &["add-project", &client.to_string_lossy()]);
    assert!(ok && out.contains("already indexed"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    // --vault puts the vault in the project too.
    let other = r.join("other");
    write(&other.join("README.md"), "# Other\n");
    let (ok, out) = settings_cmd(&p, &["add-project", "--vault", &other.to_string_lossy()]);
    assert!(ok, "{out}");
    let c = Config::load(&cfg).unwrap();
    assert!(c.vaults.is_empty());
    assert_eq!(c.user.projects.iter().find(|x| x.name == "beta").unwrap().body.vaults.len(), 2);
}

/// #74: an add-project that cannot succeed writes nothing: the
/// configuration is byte-identical and no vault folder is left behind.
#[test]
fn a_failing_add_project_leaves_the_configuration_byte_identical() {
    let t = Temp::new("proj-add-fail");
    let r = t.path();
    let (cfg, text) = two_projects(r);
    let client = r.join("client");
    write(&client.join("README.md"), "# Client\n");
    // In no project, none named, no default_project: which project is unknown.
    let p = paths(r, &cfg);
    let (ok, out) = settings_cmd(&p, &["add-project", "--vault", &client.to_string_lossy()]);
    assert!(!ok && out.contains("in no project") && out.contains("nothing written"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    assert!(!r.join("vaults").exists(), "no vault folder made");
    // Without projects and without a vault, atlas could not run on the
    // result: refused before the first write.
    let flat = r.join("flat.json");
    let flat_text = format!("{{\"db\": \"state/flat.sqlite\", \"sources\": [{{\"name\": \"beta\", \"root\": \"{}\", \"include\": [\"*.md\"]}}]}}\n", project::path_key(&r.join("beta")));
    write(&flat, &flat_text);
    let (ok, out) = settings_cmd(&paths(r, &flat), &["add-project", &client.to_string_lossy()]);
    assert!(!ok && out.contains("nothing written"), "{out}");
    assert_eq!(std::fs::read_to_string(&flat).unwrap(), flat_text);
}

/// #75: a folder a project covers (a root, its project file's folder, a
/// repository) is indexed: not offered, and add-project changes nothing.
#[test]
fn a_project_covered_folder_is_not_offered_and_add_project_no_ops() {
    let t = Temp::new("proj-covered");
    let r = t.path();
    let (cfg, _) = two_projects(r);
    let ws = r.join("ws");
    repo(&ws.join("svc"), "git@git.example:acme/svc.git");
    // Registered with --new: a project file folder and a repository.
    let p = paths(r, &cfg);
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", "--no-vault", &ws.to_string_lossy()]);
    assert!(ok, "{out}");
    let text = std::fs::read_to_string(&cfg).unwrap();
    let c = Config::load(&cfg).unwrap();
    let roots = cs::configured_roots(&c);
    for d in [r.join("alpha").join("sub"), ws.clone(), ws.join("svc")] {
        std::fs::create_dir_all(&d).unwrap();
        assert!(cs::project_line(&p, &d, &roots).is_none(), "{} is covered", d.display());
        let (ok, out) = settings_cmd(&p, &["add-project", &d.to_string_lossy()]);
        assert!(ok && out.contains("already indexed"), "{}: {out}", d.display());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    }
    // A folder no project covers is still offered.
    let loose = r.join("loose").join("thing");
    std::fs::create_dir_all(&loose).unwrap();
    assert!(cs::project_line(&p, &loose, &roots).is_some());
}

/// #76: --dry-run shows the change, the file and the project, and writes
/// nothing; a bare `add-project` (no folder, no --yes) does the same; and
/// `--project` reaches the settings command.
#[test]
fn add_project_dry_run_and_the_bare_form_write_nothing() {
    let t = Temp::new("proj-dry");
    let r = t.path();
    let (cfg, text) = two_projects(r);
    let client = r.join("client");
    write(&client.join("README.md"), "# Client\n");
    let mut p = paths(r, &cfg);
    p.project = Some("beta".into());
    let (ok, out) = settings_cmd(&p, &["add-project", "--dry-run", "--vault", &client.to_string_lossy()]);
    assert!(ok && out.contains("dry run: would write") && out.contains(&cfg.display().to_string()) && out.contains("into project beta") && out.contains("would create the vault folder"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    assert!(!r.join("vaults").exists() && !r.join("state").join("beta").join("atlas.sqlite").exists(), "nothing made");
    // --new and --to dry runs.
    let ws = r.join("ws");
    repo(&ws.join("svc"), "git@git.example:acme/svc.git");
    let (ok, out) = settings_cmd(&p, &["add-project", "--dry-run", "--new", &ws.to_string_lossy()]);
    assert!(ok && out.contains("would write") && out.contains(project::PROJECT_FILE) && out.contains("into project ws"), "{out}");
    assert!(!ws.join(project::PROJECT_FILE).exists());
    let (ok, out) = settings_cmd(&p, &["add-project", "--dry-run", "--to", "alpha", &ws.to_string_lossy()]);
    assert!(ok && out.contains("into project alpha"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    // The bare form, as a user types it, in the folder.
    let claude = r.join("claude");
    let run = |args: &[&str]| {
        let o = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
            .arg("--config")
            .arg(&cfg)
            .args(args)
            .current_dir(&client)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("ATLAS_HOME", r.join("home-atlas"))
            .env("HOME", r)
            .env("USERPROFILE", r)
            .env_remove("ATLAS_CONFIG")
            .env_remove("ATLAS_PROJECT")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    };
    let (ok, out) = run(&["--project", "beta", "settings", "add-project"]);
    assert!(ok && out.contains("writes nothing") && out.contains("usage:") && out.contains("dry run: would write") && out.contains("into project beta"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    // Named with `.`, it writes.
    let (ok, out) = run(&["--project", "beta", "settings", "add-project", "."]);
    assert!(ok && out.contains("indexed source client"), "{out}");
    assert!(Config::load(&cfg).unwrap().user.projects.iter().find(|x| x.name == "beta").unwrap().body.sources.iter().any(|s| s.name == "client"));
}

/// `atlas settings` with a person answering `answer` in a terminal.
fn settings_cmd_answering(p: &Paths, args: &[&str], answer: &str) -> (bool, String) {
    let mut out = Vec::new();
    let mut input: &[u8] = answer.as_bytes();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let code = cs::run(&args, p, &mut Io { out: &mut out, input: &mut input, interactive: true, version: None, dry_run: false });
    match code {
        Ok(c) => (c == std::process::ExitCode::SUCCESS, String::from_utf8(out).unwrap()),
        Err(e) => (false, format!("{}{e}", String::from_utf8_lossy(&out))),
    }
}

/// A folder in no project: in a terminal, add-project lists every project
/// and cancel, and writes into the one picked; one project is asked about
/// too; cancel, a dry run and no terminal write nothing.
#[test]
fn a_folder_in_no_project_is_asked_about_in_a_terminal() {
    let t = Temp::new("proj-pick");
    let r = t.path();
    let (cfg, text) = two_projects(r);
    let client = r.join("client");
    write(&client.join("README.md"), "# Client\n");
    let p = paths(r, &cfg);
    // Cancel: nothing written.
    let (ok, out) = settings_cmd_answering(&p, &["add-project", &client.to_string_lossy()], "c\n");
    assert!(ok && out.contains("1) alpha") && out.contains("2) beta") && out.contains("c) cancel") && out.contains("cancelled; nothing written"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    // A dry run is not asked: refused, nothing written.
    let (ok, out) = settings_cmd_answering(&p, &["add-project", "--dry-run", &client.to_string_lossy()], "2\n");
    assert!(!ok && out.contains("in no project") && !out.contains("c) cancel"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    // No terminal: refused, nothing written.
    let (ok, out) = settings_cmd(&p, &["add-project", &client.to_string_lossy()]);
    assert!(!ok && out.contains("in no project") && out.contains("nothing written"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
    // Picked: written into that project alone, in one edit.
    let (ok, out) = settings_cmd_answering(&p, &["add-project", &client.to_string_lossy()], "2\n");
    assert!(ok && out.contains("project beta: added source client") && out.contains("indexed source client"), "{out}");
    let c = Config::load(&cfg).unwrap();
    assert!(c.sources.is_empty() && c.user.projects.iter().find(|x| x.name == "alpha").unwrap().body.sources.is_empty());
    assert!(c.user.projects.iter().find(|x| x.name == "beta").unwrap().body.sources.iter().any(|s| s.name == "client"));
    // One project configured: still asked, never picked silently.
    let one = r.join("one.json");
    let one_text = r#"{"projects_dir": "state", "watch": false, "projects": [{"name": "solo", "roots": ["alpha"], "vaults": [{"name": "va", "path": "va"}]}]}"#;
    write(&one, one_text);
    let p1 = paths(r, &one);
    let other = r.join("other");
    write(&other.join("README.md"), "# Other\n");
    let (ok, out) = settings_cmd_answering(&p1, &["add-project", &other.to_string_lossy()], "\n");
    assert!(ok && out.contains("1) solo") && out.contains("cancelled; nothing written"), "{out}");
    assert_eq!(std::fs::read_to_string(&one).unwrap(), one_text);
}

#[test]
fn the_checkpoint_card_lists_only_the_sessions_project() {
    let two = Two::new("proj-card");
    let (ra, rb) = (two.resolve("alpha"), two.resolve("beta"));
    let (mut a, mut b) = (server_for(&ra.config), server_for(&rb.config));
    text(&mut a, "journal", json!({"lane": "ALPHA-LANE", "entry": "next", "text": "alpha work on the compactor"}));
    text(&mut b, "journal", json!({"lane": "BETA-LANE", "entry": "next", "text": "beta work on the capacitor"}));
    drop((a, b));
    for (r, own, other) in [(ra, "ALPHA-LANE", "BETA-LANE"), (rb, "BETA-LANE", "ALPHA-LANE")] {
        let mut s = settings_for(&r.config);
        s.compact.enabled = true;
        s.graph = Some(Arc::new(r.graph.clone()));
        let mut h = Hooks::open(s.clone()).unwrap();
        h.set_offer_paths(None);
        h.handle(&HookEvent { event: "PreCompact".into(), session_id: "s1".into(), trigger: "manual".into(), ..HookEvent::default() });
        let store = atlas::memory::MemoryStore::open(&s.store).unwrap();
        let ck = atlas::checkpoint::newest(&store, "s1").unwrap().expect("a checkpoint");
        assert!(ck.body.contains(own), "{own} missing from the card: {}", ck.body);
        assert!(!ck.body.contains(other), "{other} bled into the card: {}", ck.body);
    }
}

/// A fresh install has no projects: `add-project --new` in a folder of
/// repositories moves the setup into a project first (one undo reverses
/// both), and SessionStart there offers the new project, not the
/// single-repository vault question.
#[test]
fn add_project_new_on_a_fresh_install_moves_the_setup_into_projects_first() {
    let t = Temp::new("proj-fresh-new");
    let r = t.path();
    let orchard = r.join("work").join("orchard");
    repo(&orchard.join("pears"), "https://git.example/orchard/pears.git");
    repo(&orchard.join("plums"), "https://git.example/orchard/plums.git");
    // The setup moves into a project named after its configuration folder.
    let (cfg, original) = fresh_setup(&r.join("My Atlas"));
    let c = Config::load(&cfg).unwrap();
    assert!(!c.user.scoped);
    let p = paths(r, &cfg);

    let mut h = Hooks::open(settings_for(&c)).unwrap();
    h.set_offer_paths(Some(p.clone()));
    h.set_project_roots(Some(cs::configured_roots(&c)));
    let line = h.handle(&HookEvent { event: "SessionStart".into(), cwd: orchard.to_string_lossy().into_owned(), ..HookEvent::default() }).context;
    assert!(line.contains("holds 2 git repos (pears, plums)") && line.contains("no projects yet"), "{line}");
    assert!(line.contains("add-project --new") && !line.contains("add-project --vault \""), "{line}");

    let (ok, out) = settings_cmd(&p, &["add-project", "--new", &orchard.to_string_lossy()]);
    assert!(ok, "{out}");
    assert!(out.contains("moving the current setup into project my-atlas"), "{out}");
    let reg = Config::load(&cfg).unwrap();
    let names: Vec<&str> = reg.user.projects.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, vec!["my-atlas", "orchard"]);
    assert_eq!(reg.user.default_project.as_deref(), Some("my-atlas"));
    assert!(orchard.join(project::PROJECT_FILE).is_file());

    let (ok, out) = settings_cmd(&p, &["undo", "--yes"]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original, "one undo reverses the move and the project: {out}");
    assert!(!orchard.join(project::PROJECT_FILE).exists(), "{out}");
}

/// A single-scope configuration (a vault `notes`, no projects) in `home`;
/// returns the configuration file and its text.
fn fresh_setup(home: &Path) -> (PathBuf, &'static str) {
    write(&home.join("notes").join("a.md"), "# A\n");
    let cfg = home.join("atlas.json");
    let original = "{\n  \"db\": \"state/atlas.sqlite\",\n  \"vaults\": [{\"name\": \"notes\", \"path\": \"notes\"}],\n  \"sources\": [],\n  \"ids\": [],\n  \"models\": {\"auto_download\": false, \"files\": []}\n}\n";
    write(&cfg, original);
    (cfg, original)
}

fn project_names(cfg: &Path) -> Vec<String> {
    Config::load(cfg).unwrap().user.projects.iter().map(|x| x.name.clone()).collect()
}

/// Without a terminal the setup's project is named after its configuration
/// folder, `-2` when the new project takes that name; a dry run names the
/// same and writes nothing; in a terminal the user names it, and an answer
/// that is no valid name or is the new project's is asked again.
#[test]
fn the_moved_setup_is_named_after_its_folder_or_by_the_user() {
    let t = Temp::new("proj-fresh-name");
    let r = t.path();
    let orchard = r.join("work").join("orchard");
    repo(&orchard.join("pears"), "https://git.example/orchard/pears.git");

    // The configuration folder has the new project's name.
    let (cfg, original) = fresh_setup(&r.join("Orchard"));
    let p = paths(r, &cfg);
    let (ok, out) = settings_cmd(&p, &["add-project", "--dry-run", "--new", &orchard.to_string_lossy()]);
    assert!(ok && out.contains("moving the current setup into project orchard-2"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original, "a dry run writes nothing");
    let (ok, out) = settings_cmd_answering(&p, &["add-project", "--dry-run", "--new", &orchard.to_string_lossy()], "home\n");
    assert!(ok && out.contains("project orchard-2") && !out.contains("Name for the project"), "a dry run asks nothing: {out}");
    let (ok, out) = settings_cmd(&p, &["add-project", "--new", &orchard.to_string_lossy()]);
    assert!(ok, "{out}");
    assert_eq!(project_names(&cfg), ["orchard-2", "orchard"]);
    let (ok, out) = settings_cmd(&p, &["undo", "--yes"]);
    assert!(ok && std::fs::read_to_string(&cfg).unwrap() == original, "{out}");

    // In a terminal: a bad name and the new project's are refused, then
    // the answer is taken.
    let (ok, out) = settings_cmd_answering(&p, &["add-project", "--new", &orchard.to_string_lossy()], "_hidden\nOrchard\nhome-notes\n");
    assert!(ok, "{out}");
    assert!(out.contains("Name for the project the current setup moves into [orchard-2]"), "{out}");
    assert!(out.contains("\"_hidden\" is no project name") && out.contains("\"Orchard\" is the new project's name"), "{out}");
    assert!(out.contains("moving the current setup into project home-notes"), "{out}");
    assert_eq!(project_names(&cfg), ["home-notes", "orchard"]);
    let (ok, out) = settings_cmd(&p, &["undo", "--yes"]);
    assert!(ok && std::fs::read_to_string(&cfg).unwrap() == original, "{out}");

    // An empty answer, or no answer at all, takes the default.
    for answer in ["\n", ""] {
        let (ok, out) = settings_cmd_answering(&p, &["add-project", "--new", &orchard.to_string_lossy()], answer);
        assert!(ok, "{out}");
        assert_eq!(project_names(&cfg), ["orchard-2", "orchard"], "{answer:?}");
        let (ok, out) = settings_cmd(&p, &["undo", "--yes"]);
        assert!(ok && std::fs::read_to_string(&cfg).unwrap() == original, "{out}");
    }
}

/// The README's "One repository" then "A folder of repositories", as a user
/// runs them on a fresh machine: no root ends up at or above home, an unrelated
/// folder is asked about, the first project keeps its folder, and one undo
/// reverts the second step. `default_project` stays, so that folder still reads
/// the first project: the fix restores the question, not isolation.
#[test]
fn the_readme_path_leaves_unrelated_folders_asked_about() {
    let t = Temp::new("proj-wide");
    let x = t.path();
    let (p1, multi, unrelated) = (x.join("code").join("p1"), x.join("code").join("multi"), x.join("Desktop").join("unrelated"));
    write(&p1.join("README.md"), "# p1\n");
    repo(&multi.join("a"), "https://example.invalid/a.git");
    repo(&multi.join("b"), "https://example.invalid/b.git");
    std::fs::create_dir_all(&unrelated).unwrap();
    let cfg = wide_cfg(x);
    let (ok, out) = wide_atlas(x, &p1, &["settings", "add-project", "--yes", "."], None);
    assert!(ok, "{out}");
    let before = std::fs::read(&cfg).unwrap();
    let (ok, out) = wide_atlas(x, &multi, &["settings", "add-project", "--new", "."], None);
    assert!(ok, "{out}");

    let c = Config::load(&cfg).unwrap();
    assert_eq!(c.user.projects.len(), 2);
    let home = project::path_key(x);
    for p in &c.user.projects {
        for r in project::project_roots(p, &c.user.checkouts) {
            assert!(!project::key_within(&home, &project::path_key(&r)), "{} has the root {}", p.name, r.display());
        }
    }
    let first = c.user.default_project.clone().unwrap();
    let a = project::active(&c, &p1, None).unwrap().unwrap();
    assert_eq!((a.spec.name.as_str(), a.why), (first.as_str(), project::Why::Root), "the first project keeps its folder");

    let event = format!("{{\"hook_event_name\":\"SessionStart\",\"source\":\"startup\",\"cwd\":{:?}}}", unrelated.to_string_lossy());
    let (ok, line) = wide_atlas(x, &unrelated, &["--hook"], Some(&event));
    assert!(ok && line.contains("is not indexed") && line.contains("add-project --vault"), "{line}");
    let (_, again) = wide_atlas(x, &unrelated, &["--hook"], Some(&event));
    assert!(again.contains("is not indexed"), "unanswered, so asked again: {again}");
    for source in ["resume", "compact"] {
        let same = format!("{{\"hook_event_name\":\"SessionStart\",\"source\":\"{source}\",\"cwd\":{:?}}}", unrelated.to_string_lossy());
        let (_, line) = wide_atlas(x, &unrelated, &["--hook"], Some(&same));
        assert!(!line.contains("is not indexed") && !line.contains("orchestrated mode is offered"), "{source} is the same session: {line}");
    }
    let (ok, out) = wide_atlas(x, &unrelated, &["settings", "add-project", "--no", "."], None);
    assert!(ok, "{out}");
    let (_, again) = wide_atlas(x, &unrelated, &["--hook"], Some(&event));
    assert!(!again.contains("is not indexed"), "answered: {again}");
    let d = project::active(&c, &unrelated, None).unwrap().unwrap();
    assert_eq!(d.why, project::Why::Default, "default_project stays");
    // A folder indexed as a source, below it, and the starter vault are not "not indexed".
    let p3 = x.join("code").join("p3");
    write(&p3.join("docs").join("R.md"), "# p3\n");
    let (ok, out) = wide_atlas(x, &p3, &["settings", "add-project", "--yes", "."], None);
    assert!(ok, "{out}");
    let c3 = Config::load(&cfg).unwrap();
    let vault = c3.user.projects.iter().flat_map(|p| p.body.vaults.iter()).next().unwrap().path.clone();
    for dir in [p3.join("docs"), vault] {
        let event = format!("{{\"hook_event_name\":\"SessionStart\",\"source\":\"startup\",\"cwd\":{:?}}}", dir.to_string_lossy());
        let (ok, line) = wide_atlas(x, &dir, &["--hook"], Some(&event));
        assert!(ok && !line.contains("is not indexed"), "{}: {line}", dir.display());
    }
    let (ok, out) = wide_atlas(x, &p3, &["settings", "undo", "--yes"], None);
    assert!(ok, "{out}");

    let (ok, out) = wide_atlas(x, &multi, &["settings", "undo", "--yes"], None);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read(&cfg).unwrap(), before, "one undo restores the bytes");
}

/// `add-project --new` in the home folder, or above it, would write a project
/// file there, and its folder would be a root found from every folder below.
#[test]
fn add_project_new_refuses_home_and_above_and_it_is_not_offered() {
    let t = Temp::new("proj-wide-new");
    let x = t.path().join("Users").join("me");
    let p1 = x.join("code").join("p1");
    write(&p1.join("README.md"), "# p1\n");
    repo(&x.join("dotfiles"), "https://example.invalid/dotfiles.git");
    let cfg = wide_cfg(&x);
    assert!(wide_atlas(&x, &p1, &["settings", "add-project", "--yes", "."], None).0);
    let before = std::fs::read(&cfg).unwrap();
    for dir in [x.clone(), t.path().join("Users")] {
        let (ok, out) = wide_atlas(&x, &p1, &["settings", "add-project", "--new", &dir.to_string_lossy()], None);
        assert!(!ok && out.contains("home folder") && out.contains("Nothing written"), "{out}");
        assert!(!dir.join(project::PROJECT_FILE).exists());
        assert_eq!(std::fs::read(&cfg).unwrap(), before);
    }
    // A scoped configuration offers repositories in any folder that is no
    // root; home is not one of them.
    let multi = x.join("code").join("multi");
    repo(&multi.join("a"), "https://example.invalid/a.git");
    assert!(wide_atlas(&x, &multi, &["settings", "add-project", "--new", "."], None).0);
    let event = format!("{{\"hook_event_name\":\"SessionStart\",\"source\":\"startup\",\"cwd\":{:?}}}", x.to_string_lossy());
    let (_, line) = wide_atlas(&x, &x, &["--hook"], Some(&event));
    assert!(!line.contains("add-project --new") && !line.contains("is not indexed"), "{line}");
}

/// The configuration file of a fresh user whose home is `x`.
fn wide_cfg(x: &Path) -> PathBuf {
    let dir = if cfg!(windows) {
        x.join("AppData").join("Roaming").join("atlas")
    } else if cfg!(target_os = "macos") {
        x.join("Library").join("Application Support").join("atlas")
    } else {
        x.join(".config").join("atlas")
    };
    dir.join("atlas.json")
}

/// Runs the binary as a fresh user whose home is `x`.
fn wide_atlas(x: &Path, cwd: &Path, args: &[&str], stdin: Option<&str>) -> (bool, String) {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut c = Command::new(bin());
    c.args(args)
        .current_dir(cwd)
        .env("HOME", x)
        .env("USERPROFILE", x)
        .env("APPDATA", x.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", x.join("AppData").join("Local"))
        .env_remove("CLAUDE_CODE_SESSION_ATTENDED")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env("CLAUDE_CONFIG_DIR", x.join(".claude"))
        .env_remove("ATLAS_HOME")
        .env_remove("ATLAS_CONFIG")
        .env_remove(project::PROJECT_VAR)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut kid = c.spawn().unwrap();
    if let Some(text) = stdin {
        kid.stdin.as_mut().unwrap().write_all(text.as_bytes()).unwrap();
    }
    drop(kid.stdin.take());
    let o = kid.wait_with_output().unwrap();
    (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

/// Real session finding: the question was marked asked when the line was made,
/// so one a model never relayed was never asked again. It now returns until it
/// is answered, on new sessions only, and yes or no ends it.
#[test]
fn the_first_session_question_returns_until_it_is_answered() {
    let t = common::Temp::new("question");
    let r = t.path();
    let home = r.join("home");
    let dirs = [home.join("code").join("yes"), home.join("code").join("no"), home.join("code").join("other")];
    for d in &dirs {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("R.md"), "# x\n").unwrap();
    }
    let v = r.join("v");
    std::fs::create_dir_all(&v).unwrap();
    std::fs::write(v.join("N.md"), "# n\n").unwrap();
    let cfg = r.join("atlas.json");
    std::fs::write(&cfg, format!("{{\"vaults\": [{{\"name\": \"v\", \"path\": \"{}\"}}]}}\n", v.to_string_lossy().replace('\\', "/"))).unwrap();
    let p = paths(r, &cfg);
    let roots = || cs::configured_roots(&Config::load(&cfg).unwrap());
    for d in &dirs {
        let a = cs::project_line(&p, d, &roots()).unwrap();
        assert!(a.contains("--no") && a.contains("asks again next session"), "{a}");
        assert_eq!(cs::project_line(&p, d, &roots()).as_ref(), Some(&a), "a line made and not answered returns");
    }
    // No: recorded, nothing else changes.
    let before = std::fs::read_to_string(&cfg).unwrap();
    let (ok, out) = settings_cmd(&p, &["add-project", "--no", &dirs[1].to_string_lossy()]);
    assert!(ok && out.contains("will not ask"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
    assert!(cs::project_line(&p, &dirs[1], &roots()).is_none());
    // Yes: indexed, so not offered.
    let (ok, out) = settings_cmd(&p, &["add-project", &dirs[0].to_string_lossy()]);
    assert!(ok, "{out}");
    assert!(cs::project_line(&p, &dirs[0], &roots()).is_none());
    // Neither touches another folder's question.
    assert!(cs::project_line(&p, &dirs[2], &roots()).is_some());
    // --no is an answer, not a mode.
    let (ok, _) = settings_cmd(&p, &["add-project", "--no", "--new", &dirs[2].to_string_lossy()]);
    assert!(!ok);
    let (ok, _) = settings_cmd(&p, &["add-project", "--no"]);
    assert!(!ok);
}

/// A question whose answer could not be recorded would return every session, and
/// its `--no` would fail: with a state that cannot be written, atlas does not ask.
#[test]
fn a_question_that_could_not_be_recorded_is_not_asked() {
    let t = common::Temp::new("question-unwritable");
    let r = t.path();
    let home = r.join("home");
    let plain = home.join("code").join("plain");
    let acme = home.join("code").join("acme");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::create_dir_all(acme.join("a").join(".git")).unwrap();
    let cfg = r.join("atlas.json");
    std::fs::write(&cfg, "{\"vaults\": []}\n").unwrap();
    let a_file = r.join("a-file");
    std::fs::write(&a_file, "x").unwrap();
    let p = paths(r, &cfg);
    let roots = cs::configured_roots(&Config::load(&cfg).unwrap());
    let ctx = cs::OfferCtx::unscoped(&roots);
    assert!(cs::project_line(&p, &plain, &roots).is_some() && cs::repos_line(&p, &acme, &ctx).is_some() && cs::orchestrate_line(&p).is_some(), "a state that can be written asks");
    assert!(!p.state.as_ref().unwrap().exists(), "and asking writes nothing");
    let before = std::fs::read_dir(r).unwrap().count();
    for state in [Some(r.to_path_buf()), Some(a_file.join("offer.json")), None] {
        let s = Paths { state: state.clone(), ..p.clone() };
        assert!(cs::project_line(&s, &plain, &roots).is_none(), "{state:?}");
        assert!(cs::repos_line(&s, &acme, &ctx).is_none(), "{state:?}");
        assert!(cs::orchestrate_line(&s).is_none(), "{state:?}");
    }
    assert_eq!(std::fs::read_dir(r).unwrap().count(), before, "the look leaves nothing behind");
    // Windows refuses to replace a read-only file, so that state is stuck too.
    #[cfg(windows)]
    {
        let held = r.join("held.json");
        std::fs::write(&held, "{}\n").unwrap();
        let mut perm = std::fs::metadata(&held).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&held, perm.clone()).unwrap();
        let s = Paths { state: Some(held.clone()), ..p.clone() };
        assert!(cs::project_line(&s, &plain, &roots).is_none());
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&held, perm).unwrap();
        assert!(cs::project_line(&s, &plain, &roots).is_some());
    }
}

/// With projects and none for a plain folder, the question's yes must name one or
/// it is refused; `--to` with nothing to add and `orchestrate --no` are answers too,
/// and one that cannot be recorded says so.
#[test]
fn every_answer_to_the_question_ends_it_or_says_why_not() {
    let t = common::Temp::new("answers");
    let r = t.path();
    let plain = r.join("plain");
    let acme = r.join("acme");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::create_dir_all(acme.join("a").join(".git")).unwrap();
    let cfg = r.join("atlas.json");
    std::fs::write(&cfg, r#"{"projects_dir": "state", "projects": [{"name": "solo", "roots": ["solo"], "repos": [{"name": "a", "path": "elsewhere/a"}]}, {"name": "two", "roots": ["two"]}], "watch": false}"#).unwrap();
    let p = paths(r, &cfg);
    let roots = || cs::configured_roots(&Config::load(&cfg).unwrap());
    let line = cs::project_line(&p, &plain, &roots()).unwrap();
    assert!(line.contains("atlas --project NAME settings add-project --vault \"") && line.contains("the user picks: solo, two."), "{line}");
    let dir = plain.to_string_lossy().into_owned();
    let (ok, _) = settings_cmd(&p, &["add-project", "--vault", &dir]);
    assert!(!ok, "no project named, so refused");
    assert!(cs::project_line(&p, &plain, &roots()).is_some());
    let named = Paths { project: Some("solo".into()), ..p.clone() };
    let (ok, out) = settings_cmd(&named, &["add-project", "--vault", &dir]);
    assert!(ok, "{out}");
    assert!(cs::project_line(&p, &plain, &roots()).is_none(), "the named yes ends it");
    // Every repo there is in the project by name: nothing to add, and still an answer.
    let (ok, out) = settings_cmd(&p, &["add-project", "--to", "solo", &acme.to_string_lossy()]);
    assert!(ok && out.contains("nothing to change"), "{out}");
    let state = std::fs::read_to_string(p.state.as_ref().unwrap()).unwrap();
    assert!(state.contains("acme"), "{state}");
    // An answer that cannot be recorded is no success.
    let stuck = Paths { state: Some(r.to_path_buf()), ..p.clone() };
    let (ok, out) = settings_cmd(&stuck, &["orchestrate", "--no"]);
    assert!(!ok && out.contains("could not record"), "{out}");
}
