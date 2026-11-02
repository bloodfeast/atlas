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
    assert!(!session_start(&sb).contains("git repos"), "asked once for every project");
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
    Paths { claude_dir: root.join("claude"), atlas_config: Some(config.to_path_buf()), state: Some(root.join("state").join(cs::OFFER_STATE)), db: Some(root.join("state").join("atlas.sqlite")), user_state: Some(root.join("state")) }
}

fn settings_cmd(p: &Paths, args: &[&str]) -> (bool, String) {
    let mut out = Vec::new();
    let mut input: &[u8] = b"";
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let code = cs::run(&args, p, &mut Io { out: &mut out, input: &mut input, interactive: false, version: None });
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
    let (ok, out) = settings_cmd(&p, &["undo"]);
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
    assert!(!session_start(&p).contains("git repos"), "asked once per folder");

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
    let (ok, out) = settings_cmd(&p, &["undo"]);
    assert!(ok && !pf.exists(), "undo removes the project file: {out}");
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

    // The offer: asked once for the folder, whichever way it is reached.
    let p = paths(r, &cfg);
    let session_start = |cwd: &Path| {
        let mut h = Hooks::open(settings_for(&project::resolve(&c, cwd, None).unwrap().config)).unwrap();
        h.set_offer_paths(Some(p.clone()));
        h.set_offer_ctx(Some(OfferCtx::new(&c, &project::Graph::default())));
        h.handle(&HookEvent { event: "SessionStart".into(), cwd: cwd.to_string_lossy().into_owned(), ..HookEvent::default() }).context
    };
    let line = session_start(&via);
    assert!(line.contains("holds 2 git repos (auth, billing)"), "{line}");
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
