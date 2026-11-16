//! The worktree ownership ledger (`src/worktree_ledger.rs`): one row per
//! ownership span, written at admitted Agent launches from `Uses:
//! worktree:NAME`, against temp stores and temp git repositories with real
//! `git worktree add` worktrees. Nothing here touches a live store.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use atlas::hooks::{HookAnswer, HookEvent, Hooks};
use atlas::json;
use atlas::memory::settings::Settings;
use atlas::memory::MemoryStore;
use atlas::orchestrate::{Kind, KindSpec, OrchSettings, OrchestrateSpec};
use atlas::project::{Graph, Repo};
use atlas::worktree_ledger::{self as wl, Launch, Owner, Row, Worktree, Wrote};
use common::*;

/// Runs git in `dir` with a throwaway identity and no hooks.
fn git(dir: &Path, args: &[&str]) -> String {
    let nohooks = dir.join(".no-hooks");
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=atlas-test", "-c", "user.email=atlas-test", "-c", "commit.gpgsign=false"])
        .arg("-c")
        .arg(format!("core.hooksPath={}", nohooks.display()))
        .args(args)
        .output()
        .expect("run git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A repository with one commit, and its worktrees folder.
struct Fixture {
    _t: Temp,
    root: PathBuf,
    repo: PathBuf,
    wts: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let t = Temp::new(tag);
        let root = t.path().to_path_buf();
        let repo = root.join("atlas");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let wts = root.join("atlas-worktrees");
        std::fs::create_dir_all(&wts).unwrap();
        Fixture { _t: t, root, repo, wts }
    }

    /// `git worktree add` at `atlas-worktrees/NAME` on a new branch NAME.
    fn add(&self, name: &str) -> PathBuf {
        let p = self.wts.join(name);
        git(&self.repo, &["worktree", "add", "-q", "-b", name, &p.to_string_lossy()]);
        p
    }

    fn repos(&self) -> Vec<Repo> {
        vec![Repo { name: "atlas".into(), path: Some(self.repo.clone()), ..Repo::default() }]
    }

    fn store_path(&self) -> PathBuf {
        self.root.join("state").join("memory.sqlite")
    }

    fn store(&self) -> MemoryStore {
        MemoryStore::open(&self.store_path()).unwrap()
    }

    /// A worktree as git lists it.
    fn wt(&self, name: &str) -> Worktree {
        let (all, errors) = wl::worktrees(&self.repos(), &[]);
        assert!(errors.is_empty(), "{errors:?}");
        all.into_iter().find(|w| w.path.ends_with(name)).unwrap_or_else(|| panic!("no worktree {name}"))
    }
}

fn on() -> OrchSettings {
    atlas::orchestrate::resolve(Some(&OrchestrateSpec { enabled: Some(true), ..Default::default() }), None, None, |_| None)
}

fn brief(kind: &str, lane: Option<&str>, uses: &str) -> String {
    // The body names the lane and the worktree: the queue retires a waiting
    // item whose brief is the same, so two launches here never share one.
    let body = format!("Do the work of {} on {uses}.", lane.unwrap_or("no lane"));
    let lane = lane.map_or(String::new(), |l| format!("Lane: {l}\n"));
    format!("Kind: {kind}\n{lane}Uses: {uses}\n\n{body}")
}

/// [`wl::at_launch`] as the hook calls it.
fn launch_at(f: &Fixture, s: &MemoryStore, kind: Kind, b: &str, id: &str) -> wl::Done {
    wl::at_launch(s, &on(), &f.repos(), &Launch { brief: b, kind, description: "the job", tool_use_id: id, session: "s1" })
}

fn all_rows(s: &MemoryStore) -> Vec<Row> {
    let mut st = s.conn().prepare("SELECT DISTINCT path FROM worktree_owners").unwrap();
    let paths: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    let mut v: Vec<Row> = paths.iter().flat_map(|p| wl::history(s.conn(), p).unwrap()).collect();
    v.sort_by_key(|r| r.id);
    v
}

#[test]
fn first_launch_on_a_worktree_writes_a_row() {
    let f = Fixture::new("wl-first");
    let p = f.add("atlas-wt-own");
    let s = f.store();
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", Some("ATLAS-USES"), "compile, worktree:atlas-wt-own"), "tu1");
    assert!(d.errors.is_empty(), "{d:?}");
    assert!(matches!(d.wrote[..], [(ref n, Wrote::Opened(_))] if n == "atlas-wt-own"), "{d:?}");
    let rows = all_rows(&s);
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    // git prints the resolved path (macOS: /var is a link to /private/var).
    assert_eq!(r.path_key, wl::path_key(&std::fs::canonicalize(&p).unwrap().to_string_lossy()));
    assert_eq!(wl::path_key(&r.path), r.path_key);
    assert_eq!((r.repo.as_str(), r.branch.as_str(), r.lane.as_str()), ("atlas", "atlas-wt-own", "ATLAS-USES"));
    assert_eq!((r.description.as_str(), r.tool_use_id.as_str(), r.session.as_str()), ("the job", "tu1", "s1"));
    assert_eq!(r.created_at, r.last_modified);
    assert_eq!((r.removed_at.as_deref(), r.ended_at.as_deref()), (None, None));
}

#[test]
fn a_same_lane_relaunch_only_bumps_last_modified() {
    let f = Fixture::new("wl-bump");
    f.add("x");
    let s = f.store();
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("Parser"), "worktree:x"), "tu1");
    let before = all_rows(&s);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", Some("PARSER"), "worktree:x"), "tu2");
    assert!(matches!(d.wrote[..], [(_, Wrote::Bumped(_))]), "{d:?}");
    let after = all_rows(&s);
    assert_eq!(after.len(), 1, "no row is added");
    assert!(after[0].last_modified > before[0].last_modified, "bumped");
    assert_eq!(Row { last_modified: before[0].last_modified.clone(), ..after[0].clone() }, before[0], "no other field changes");
}

#[test]
fn a_review_or_read_only_launch_never_takes_ownership() {
    let f = Fixture::new("wl-review");
    f.add("x");
    f.add("y");
    let s = f.store();
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:x"), "tu1");
    let before = all_rows(&s);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let d = launch_at(&f, &s, Kind::Reviewer, &brief("reviewer", Some("L2"), "worktree:x"), "tu2");
    assert!(matches!(d.wrote[..], [(_, Wrote::Bumped(_))]), "{d:?}");
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L2"), "read-only, worktree:x"), "tu3");
    assert!(matches!(d.wrote[..], [(_, Wrote::Bumped(_))]), "{d:?}");
    let after = all_rows(&s);
    assert_eq!(after.len(), 1, "no row added or closed");
    assert_eq!(after[0].lane, "L1");
    assert_eq!(after[0].ended_at, None);
    assert!(after[0].last_modified > before[0].last_modified);
    // With no open row a review writes nothing.
    let d = launch_at(&f, &s, Kind::Reviewer, &brief("reviewer", Some("L2"), "worktree:y"), "tu4");
    assert!(matches!(d.wrote[..], [(_, Wrote::Nothing)]), "{d:?}");
    assert_eq!(all_rows(&s).len(), 1);
}

#[test]
fn a_launch_with_no_lane_bumps_or_opens_an_unknown_row() {
    let f = Fixture::new("wl-nolane");
    f.add("x");
    f.add("y");
    let s = f.store();
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:x"), "tu1");
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", None, "worktree:x"), "tu2");
    assert!(matches!(d.wrote[..], [(_, Wrote::Bumped(_))]), "{d:?}");
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", None, "worktree:y"), "tu3");
    assert!(matches!(d.wrote[..], [(_, Wrote::Opened(_))]), "{d:?}");
    let rows = all_rows(&s);
    assert_eq!(rows.iter().map(|r| r.lane.as_str()).collect::<Vec<_>>(), ["L1", wl::UNKNOWN]);
}

#[test]
fn a_launch_by_another_owner_closes_the_span_and_opens_one() {
    let f = Fixture::new("wl-handover");
    f.add("x");
    let s = f.store();
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:x"), "tu1");
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L2"), "worktree:x"), "tu2");
    assert!(matches!(d.wrote[..], [(_, Wrote::HandedOver { .. })]), "{d:?}");
    let rows = all_rows(&s);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].lane, "L1");
    assert!(rows[0].ended_at.is_some() && rows[0].removed_at.is_none(), "closed by ended_at, the worktree still exists");
    let open = wl::open_rows(s.conn()).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].lane, "L2");
}

/// The hook's settings: orchestrate on, the fixture's repository as the
/// project graph.
fn hook_settings(f: &Fixture, o: &OrchestrateSpec) -> Settings {
    let mut s = Settings::from_config(None, &f.root.join("state").join("index.sqlite")).unwrap();
    s.snapshot_secs = 0;
    s.orchestrate = atlas::orchestrate::resolve(Some(o), None, None, |_| None);
    s.graph = Some(Arc::new(Graph { name: "p".into(), repos: f.repos(), ..Graph::default() }));
    assert_eq!(s.store, f.store_path());
    s
}

fn hook_launch(h: &mut Hooks, transcript: &Path, id: &str, prompt: &str) -> HookAnswer {
    h.handle(&HookEvent {
        event: "PreToolUse".into(),
        session_id: "s1".into(),
        tool_name: "Agent".into(),
        tool_use_id: id.into(),
        transcript_path: transcript.to_string_lossy().into_owned(),
        tool_prompt: prompt.into(),
        tool_input: json!({"description": "d", "subagent_type": "general-purpose", "prompt": prompt}),
        ..HookEvent::default()
    })
}

/// A running agent of the session, as Claude Code records it.
fn running(f: &Fixture, id: &str, tool_use_id: &str, brief: &str) -> PathBuf {
    let parent = f.root.join("proj").join("sess.jsonl");
    let dir = f.root.join("proj").join("sess").join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    if !parent.exists() {
        std::fs::write(&parent, "").unwrap();
    }
    if !id.is_empty() {
        let meta = json!({"agentType": "general-purpose", "isBuiltIn": true, "description": format!("job {id}"), "toolUseId": tool_use_id, "model": "opus"});
        std::fs::write(dir.join(format!("agent-{id}.meta.json")), meta.to_string()).unwrap();
        let user = json!({"type": "user", "message": {"role": "user", "content": brief}});
        let usage = json!({"type": "assistant", "message": {"id": "m1", "usage": {"input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 998, "output_tokens": 0}}});
        std::fs::write(dir.join(format!("agent-{id}.jsonl")), format!("{user}\n{usage}\n")).unwrap();
    }
    parent
}

#[test]
fn the_hook_records_an_admitted_launch_and_nothing_for_a_denied_or_queued_one() {
    let f = Fixture::new("wl-hook");
    f.add("x");
    f.add("y");
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(Kind::Builder, KindSpec { max: Some(2), ..Default::default() })],
        ..Default::default()
    };
    let tp = running(&f, "", "", "");
    let mut h = Hooks::open(hook_settings(&f, &o)).unwrap();
    // File times are coarser than the process clock.
    std::thread::sleep(std::time::Duration::from_millis(50));
    // A running builder (started after this process) holds lane L1 and worktree y.
    running(&f, "b1", "t-b1", &brief("builder", Some("L1"), "worktree:y"));
    // The queue denies a launch declaring the held worktree; nothing is written.
    let a = hook_launch(&mut h, &tp, "t1", &brief("builder", Some("L9"), "worktree:y"));
    let r = a.deny.expect("a declared worktree conflict is denied and queued");
    assert!(r.contains("worktree:y"), "{r}");
    assert!(wl::open_rows(h.store().conn()).unwrap().is_empty(), "a queued launch writes nothing");
    // Admitted: written, and the answer is what it would be without the ledger.
    let a = hook_launch(&mut h, &tp, "t2", &brief("builder", Some("L2"), "worktree:x"));
    assert!(a.deny.is_none() && a.context.is_empty(), "{a:?}");
    let open = wl::open_rows(h.store().conn()).unwrap();
    assert_eq!(
        open.iter().map(|r| (r.lane.as_str(), r.tool_use_id.as_str(), r.session.as_str())).collect::<Vec<_>>(),
        [("L2", "t2", "s1")]
    );
    // The kind cap (b1 and t2 hold both builder slots) denies; nothing is written.
    let a = hook_launch(&mut h, &tp, "t3", &brief("builder", Some("L3"), "worktree:x"));
    assert!(a.deny.as_deref().is_some_and(|r| r.contains("2 of 2 builder")), "{a:?}");
    assert_eq!(all_rows(h.store()).len(), 1, "a denied launch changes nothing");
    assert_eq!(wl::open_rows(h.store().conn()).unwrap()[0].lane, "L2");
}

#[test]
fn an_inferred_or_unknown_worktree_is_not_recorded_and_never_fails_the_launch() {
    let f = Fixture::new("wl-inferred");
    let p = f.add("x");
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(Kind::Builder, KindSpec { max: Some(6), ..Default::default() })],
        ..Default::default()
    };
    let tp = running(&f, "", "", "");
    let mut h = Hooks::open(hook_settings(&f, &o)).unwrap();
    let named = format!("Kind: builder\nLane: L1\n\nWork in {}/worktrees/x and in {}.", f.root.display(), p.display());
    let a = hook_launch(&mut h, &tp, "t1", &named);
    assert!(a.deny.is_none(), "{a:?}");
    assert!(all_rows(h.store()).is_empty(), "a worktree the brief only names is not recorded");
    let a = hook_launch(&mut h, &tp, "t2", &brief("builder", Some("L1"), "worktree:nosuch"));
    assert!(a.deny.is_none(), "{a:?}");
    assert!(all_rows(h.store()).is_empty());
    let s = f.store();
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:nosuch"), "t3");
    assert!(d.wrote.is_empty() && d.errors.iter().any(|e| e.contains("worktree:nosuch names no worktree")), "{d:?}");
    // Outside orchestrate.worktree_roots a worktree is unknown too.
    let mut orch = on();
    orch.worktree_roots = vec![f.root.join("elsewhere")];
    let d = wl::at_launch(
        &s,
        &orch,
        &f.repos(),
        &Launch {
            brief: &brief("builder", Some("L1"), "worktree:x"),
            kind: Kind::Builder,
            description: "d",
            tool_use_id: "t4",
            session: "s1",
        },
    );
    assert!(d.wrote.is_empty() && !d.errors.is_empty(), "{d:?}");
    orch.worktree_roots = vec![f.wts.clone()];
    let d = wl::at_launch(
        &s,
        &orch,
        &f.repos(),
        &Launch {
            brief: &brief("builder", Some("L1"), "worktree:x"),
            kind: Kind::Builder,
            description: "d",
            tool_use_id: "t5",
            session: "s1",
        },
    );
    assert!(matches!(d.wrote[..], [(_, Wrote::Opened(_))]), "{d:?}");
}

/// A root configured through a symlink to the real worktrees folder still
/// holds its worktrees: git prints resolved paths (on macOS every temp dir
/// is under the `/var` -> `/private/var` link), so roots are matched resolved.
#[cfg(unix)]
#[test]
fn a_root_configured_through_a_symlink_resolves_its_worktree() {
    let f = Fixture::new("wl-symlink-root");
    f.add("x");
    let link = f.root.join("wts-link");
    std::os::unix::fs::symlink(&f.wts, &link).unwrap();
    let (all, errors) = wl::worktrees(&f.repos(), std::slice::from_ref(&link));
    assert!(errors.is_empty(), "{errors:?}");
    assert!(all.iter().any(|w| w.path.ends_with("/x")), "{all:?}");
    let mut orch = on();
    orch.worktree_roots = vec![link];
    let s = f.store();
    let d = wl::at_launch(
        &s,
        &orch,
        &f.repos(),
        &Launch {
            brief: &brief("builder", Some("L1"), "worktree:x"),
            kind: Kind::Builder,
            description: "d",
            tool_use_id: "t1",
            session: "s1",
        },
    );
    assert!(matches!(d.wrote[..], [(_, Wrote::Opened(_))]), "{d:?}");
}

/// The 8.3 short name of `p` (`GetShortPathNameW`), or `None` when it has
/// none that differs (8.3 names off on the volume, or every segment short).
#[cfg(windows)]
fn short_name(p: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    #[link(name = "kernel32")]
    extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, len: u32) -> u32;
    }
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: `wide` is NUL-terminated and `buf` holds `buf.len()` u16s.
    let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), u32::try_from(buf.len()).unwrap()) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let short = PathBuf::from(OsString::from_wide(&buf[..n]));
    (wl::path_key(&short.to_string_lossy()) != wl::path_key(&p.to_string_lossy())).then_some(short)
}

/// A root configured by its 8.3 short name (as `%TEMP%` is on the GitHub
/// Windows runners, `RUNNER~1`) still holds its worktrees: git prints long
/// names, so roots are matched resolved.
#[cfg(windows)]
#[test]
fn a_root_configured_by_its_short_name_resolves_its_worktree() {
    let f = Fixture::new("wl-short-root");
    f.add("x");
    let Some(short) = short_name(&f.wts) else {
        eprintln!("skipped: no 8.3 short name for {} (8.3 names off on this volume)", f.wts.display());
        return;
    };
    let (all, errors) = wl::worktrees(&f.repos(), std::slice::from_ref(&short));
    assert!(errors.is_empty(), "{errors:?}");
    assert!(all.iter().any(|w| w.path.ends_with("/x")), "{short:?}: {all:?}");
    let mut orch = on();
    orch.worktree_roots = vec![short];
    let s = f.store();
    let d = wl::at_launch(
        &s,
        &orch,
        &f.repos(),
        &Launch {
            brief: &brief("builder", Some("L1"), "worktree:x"),
            kind: Kind::Builder,
            description: "d",
            tool_use_id: "t1",
            session: "s1",
        },
    );
    assert!(matches!(d.wrote[..], [(_, Wrote::Opened(_))]), "{d:?}");
}

#[test]
fn orchestrate_off_writes_nothing() {
    let f = Fixture::new("wl-off");
    f.add("x");
    let tp = running(&f, "", "", "");
    let mut h = Hooks::open(hook_settings(&f, &OrchestrateSpec::default())).unwrap();
    let a = hook_launch(&mut h, &tp, "t1", &brief("builder", Some("L1"), "worktree:x"));
    assert!(a.deny.is_none());
    assert!(all_rows(h.store()).is_empty());
}

#[test]
fn a_worktree_that_has_gone_is_stamped_removed_and_nothing_else_changes() {
    let f = Fixture::new("wl-gone");
    let x = f.add("x");
    let y = f.add("y");
    let s = f.store();
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:x"), "tu1");
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:y"), "tu2");
    let listed_before = git(&f.repo, &["worktree", "list", "--porcelain"]);
    // On demand, a folder that is gone but still in git's list is not stamped.
    std::fs::remove_dir_all(&y).unwrap();
    let out = wl::run(&["list".into()], &s, &on(), &f.repos()).unwrap();
    assert!(!out.contains("stamped removed"), "{out}");
    assert_eq!(wl::open_rows(s.conn()).unwrap().len(), 2);
    // Gone from both: stamped by the list.
    git(&f.repo, &["worktree", "remove", "--force", &x.to_string_lossy()]);
    let out = wl::run(&["list".into()], &s, &on(), &f.repos()).unwrap();
    assert!(out.contains("1 row stamped removed"), "{out}");
    // At the next launch the stat alone stamps y.
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "none"), "tu3");
    assert_eq!(d.removed.len(), 1, "{d:?}");
    let rows = all_rows(&s);
    assert_eq!(rows.len(), 2, "no row is deleted");
    assert!(rows.iter().all(|r| r.removed_at.is_some() && r.ended_at.is_none()));
    // Nothing in git changed but the remove this test ran.
    let listed_after = git(&f.repo, &["worktree", "list", "--porcelain"]);
    assert!(listed_after.contains("/y") || listed_after.contains("\\y"), "y is still in git's list: {listed_after}");
    assert!(listed_before.len() > listed_after.len());
}

#[test]
fn a_re_created_path_is_a_new_span() {
    let f = Fixture::new("wl-recreate");
    let x = f.add("x");
    let s = f.store();
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:x"), "tu1");
    git(&f.repo, &["worktree", "remove", "--force", &x.to_string_lossy()]);
    git(&f.repo, &["branch", "-q", "-D", "x"]);
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "none"), "tu2");
    assert_eq!(d.removed.len(), 1);
    let old = all_rows(&s);
    f.add("x");
    let d = launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:x"), "tu3");
    assert!(matches!(d.wrote[..], [(_, Wrote::Opened(_))]), "{d:?}");
    let rows = all_rows(&s);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], old[0], "the old row is unchanged");
    assert!(rows[1].removed_at.is_none() && rows[1].ended_at.is_none());
}

#[test]
fn paths_match_without_case_on_windows_only() {
    let f = Fixture::new("wl-case");
    let s = f.store();
    let o = Owner { lane: Some("L1"), holds: true, description: "d", tool_use_id: "t", session: "s" };
    let w = Worktree { path: "D:/src/atlas-worktrees/X".into(), repo: "atlas".into(), branch: "X".into() };
    wl::record(s.conn(), s.path(), &w, &o, &atlas::memory::now()).unwrap();
    let w2 = Worktree { path: r"d:\src\atlas-worktrees\x\".into(), ..w.clone() };
    let got = wl::record(s.conn(), s.path(), &w2, &o, &atlas::memory::now()).unwrap();
    if cfg!(windows) {
        assert!(matches!(got, Wrote::Bumped(_)), "{got:?}");
        assert_eq!(wl::path_key(r"\\?\D:\src\atlas-worktrees\X"), "d:/src/atlas-worktrees/x");
    } else {
        assert!(matches!(got, Wrote::Opened(_)), "{got:?}");
        assert_eq!(wl::path_key("/home/u/Wt/"), "/home/u/Wt");
    }
    // A path differing only by case on a case-sensitive system.
    let w3 = Worktree { path: "D:/src/atlas-worktrees/x".into(), ..w.clone() };
    let got = wl::record(s.conn(), s.path(), &w3, &o, &atlas::memory::now()).unwrap();
    assert_eq!(matches!(got, Wrote::Bumped(_)), cfg!(windows), "{got:?}");
    assert_eq!(wl::path_key("C:/"), if cfg!(windows) { "c:/" } else { "C:/" });
}

#[test]
fn two_launches_at_once_leave_one_open_row() {
    let f = Fixture::new("wl-race");
    f.add("x");
    let w = f.wt("x");
    let path = f.store_path();
    drop(f.store());
    for round in 0..12 {
        // Odd rounds: two owners; even rounds: the same lane twice.
        let lanes: [String; 2] =
            if round % 2 == 1 { [format!("A{round}"), format!("B{round}")] } else { [format!("S{round}"), format!("s{round}")] };
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = lanes
            .iter()
            .map(|lane| {
                let (barrier, path, w, lane) = (barrier.clone(), path.clone(), w.clone(), lane.clone());
                std::thread::spawn(move || {
                    let s = MemoryStore::open(&path).unwrap();
                    let o = Owner { lane: Some(&lane), holds: true, description: "d", tool_use_id: "t", session: "s" };
                    barrier.wait();
                    wl::record(s.conn(), s.path(), &w, &o, &atlas::memory::now()).unwrap()
                })
            })
            .collect();
        let got: Vec<Wrote> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let s = MemoryStore::open(&path).unwrap();
        let open = wl::open_rows(s.conn()).unwrap();
        assert_eq!(open.len(), 1, "round {round}: {got:?}");
        if round % 2 == 1 {
            assert!(
                got.iter().any(|g| matches!(g, Wrote::HandedOver { .. })),
                "round {round}: the later one closes the earlier span: {got:?}"
            );
            let h = wl::history(s.conn(), &w.path).unwrap();
            let this: Vec<&Row> = h.iter().filter(|r| lanes.contains(&r.lane)).collect();
            assert_eq!(this.len(), 2);
            assert_eq!(this.iter().filter(|r| r.ended_at.is_some()).count(), 1);
        } else {
            assert!(got.iter().any(|g| matches!(g, Wrote::Bumped(_))), "round {round}: {got:?}");
        }
    }
}

#[test]
fn list_shows_open_rows_and_history_shows_every_row() {
    let f = Fixture::new("wl-list");
    f.add("x");
    f.add("y");
    let s = f.store();
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:x"), "tu1");
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L2"), "worktree:x"), "tu2");
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L3"), "worktree:x"), "tu3");
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L4"), "worktree:y"), "tu4");
    let open = wl::open_rows(s.conn()).unwrap();
    let out = wl::run(&["list".into()], &s, &on(), &f.repos()).unwrap();
    assert_eq!(out.lines().count(), 2, "{out}");
    for r in &open {
        let line = out.lines().find(|l| l.starts_with(&r.path)).unwrap_or_else(|| panic!("{} in {out}", r.path));
        assert!(
            line.contains(&format!("lane {}", r.lane)) && line.contains("0 min old") && line.contains(&format!("last {}", r.last_modified)),
            "{line}"
        );
    }
    assert!(!out.contains("lane L1") && !out.contains("lane L2"), "no closed row: {out}");
    let x = f.wt("x");
    let out = wl::run(&["history".into(), x.path.clone()], &s, &on(), &f.repos()).unwrap();
    let lanes: Vec<&str> = out.lines().map(|l| l.split("lane ").nth(1).unwrap().split(' ').next().unwrap()).collect();
    assert_eq!(lanes, ["L1", "L2", "L3"], "{out}");
    assert_eq!(out.lines().filter(|l| l.contains("  ended ")).count(), 2, "{out}");
    let h = wl::history(s.conn(), &x.path).unwrap();
    assert!(h.windows(2).all(|p| p[0].created_at <= p[1].created_at));
}

#[test]
fn the_backfill_opens_one_row_per_worktree_and_is_idempotent() {
    let f = Fixture::new("wl-backfill");
    f.add("a");
    f.add("b");
    let gone = f.add("c");
    std::fs::remove_dir_all(&gone).unwrap();
    let s = f.store();
    // b already has an owner: left untouched.
    launch_at(&f, &s, Kind::Builder, &brief("builder", Some("L1"), "worktree:b"), "tu1");
    let before_b = all_rows(&s);
    let out = wl::run(&["backfill".into()], &s, &on(), &f.repos()).unwrap();
    assert!(out.starts_with("1 row written; 2 worktrees found"), "{out}");
    let rows = all_rows(&s);
    assert_eq!(rows.len(), 2, "one row per worktree on disk; the main checkout and the missing c have none");
    let a = rows.iter().find(|r| r.branch == "a").unwrap();
    assert_eq!((a.lane.as_str(), a.tool_use_id.as_str(), a.session.as_str(), a.description.as_str()), (wl::UNKNOWN, "", "", wl::BACKFILL));
    assert!(rows.contains(&before_b[0]), "the open row is untouched");
    let out = wl::run(&["backfill".into()], &s, &on(), &f.repos()).unwrap();
    assert!(out.starts_with("0 rows written"), "{out}");
    assert_eq!(all_rows(&s), rows);
}

#[test]
fn the_triggers_refuse_deletes_rewrites_and_second_stamps() {
    let f = Fixture::new("wl-triggers");
    let s = f.store();
    let o = Owner { lane: Some("L1"), holds: true, description: "d", tool_use_id: "t", session: "s" };
    let w = Worktree { path: "/w/x".into(), repo: "atlas".into(), branch: "x".into() };
    wl::record(s.conn(), s.path(), &w, &o, "2026-01-01T00:00:00.000Z").unwrap();
    let c = s.conn();
    let refused = |sql: &str| {
        let e = c.execute_batch(sql).expect_err(sql).to_string();
        assert!(e.contains("worktree_owners"), "{sql}: {e}");
    };
    refused("DELETE FROM worktree_owners");
    for col in ["path", "path_key", "repo", "branch", "lane", "description", "tool_use_id", "session", "created_at", "id"] {
        refused(&format!("UPDATE worktree_owners SET {col} = {col}"));
    }
    // last_modified may move on an open row; a stamp is set once.
    c.execute_batch("UPDATE worktree_owners SET last_modified = '2026-01-02T00:00:00.000Z'").unwrap();
    c.execute_batch("UPDATE worktree_owners SET removed_at = '2026-01-03T00:00:00.000Z'").unwrap();
    refused("UPDATE worktree_owners SET removed_at = '2026-01-04T00:00:00.000Z'");
    refused("UPDATE worktree_owners SET removed_at = NULL");
    refused("UPDATE worktree_owners SET last_modified = '2026-01-05T00:00:00.000Z'");
    // ended_at the same way, on a second row.
    let w2 = Worktree { path: "/w/y".into(), ..w };
    wl::record(c, s.path(), &w2, &o, "2026-01-01T00:00:00.000Z").unwrap();
    c.execute_batch("UPDATE worktree_owners SET ended_at = '2026-01-03T00:00:00.000Z' WHERE path = '/w/y'").unwrap();
    refused("UPDATE worktree_owners SET ended_at = '2026-01-04T00:00:00.000Z' WHERE path = '/w/y'");
    refused("UPDATE worktree_owners SET ended_at = NULL WHERE path = '/w/y'");
    // The partial unique index: one open row per path.
    c.execute_batch(
        "INSERT INTO worktree_owners(path, path_key, repo, branch, lane, description, tool_use_id, session, created_at, last_modified) VALUES('/w/z', '/w/z', 'a', 'z', 'L', '', '', '', 't', 't')",
    )
    .unwrap();
    let e = c
        .execute_batch("INSERT INTO worktree_owners(path, path_key, repo, branch, lane, description, tool_use_id, session, created_at, last_modified) VALUES('/w/z', '/w/z', 'a', 'z', 'M', '', '', '', 't', 't')")
        .expect_err("a second open row");
    assert!(e.to_string().contains("UNIQUE"), "{e}");
    let n: i64 = c.query_row("SELECT COUNT(*) FROM worktree_owners", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 3);
}

#[test]
fn porcelain_parsing_leaves_out_the_main_checkout_and_bare_entries() {
    let text = "worktree /r/main\nHEAD 1\nbranch refs/heads/main\n\nworktree /r/wt/a\nHEAD 2\nbranch refs/heads/feat/a\n\nworktree /r/wt/d\nHEAD 3\ndetached\nprunable gitdir file points to non-existent location\n\n";
    let v = wl::parse_porcelain(text, "r");
    assert_eq!(
        v,
        [
            Worktree { path: "/r/wt/a".into(), repo: "r".into(), branch: "feat/a".into() },
            Worktree { path: "/r/wt/d".into(), repo: "r".into(), branch: String::new() }
        ]
    );
    assert!(wl::parse_porcelain("worktree /r/bare\nbare\n\n", "r").is_empty());
    assert_eq!(wl::declared_worktrees("Kind: builder\nUses: compile, worktree:Atlas-WT-Own, gpu\n"), ["atlas-wt-own"]);
}

/// The hook time the ledger adds: [`wl::at_launch`] on a launch with and
/// without a declared worktree, medians of 15 runs, printed for the record.
#[test]
fn the_added_hook_time_is_measured() {
    let f = Fixture::new("wl-latency");
    for i in 0..4 {
        f.add(&format!("w{i}"));
    }
    // Three repositories, as the live project lists.
    let mut repos = f.repos();
    for n in ["rs", "ridge"] {
        let r = f.root.join(n);
        std::fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q", "-b", "main"]);
        git(&r, &["commit", "-q", "--allow-empty", "-m", "init"]);
        repos.push(Repo { name: n.into(), path: Some(r), ..Repo::default() });
    }
    let s = f.store();
    let at = |b: &str, id: &str| {
        wl::at_launch(&s, &on(), &repos, &Launch { brief: b, kind: Kind::Builder, description: "d", tool_use_id: id, session: "s1" })
    };
    at(&brief("builder", Some("L1"), "worktree:w0"), "warm");
    let median = |b: &str| {
        let mut v: Vec<f64> = (0..15)
            .map(|i| {
                let t = std::time::Instant::now();
                let d = at(b, &format!("t{i}"));
                assert!(d.errors.is_empty(), "{d:?}");
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let without = median(&brief("builder", Some("L1"), "compile"));
    let with = median(&brief("builder", Some("L1"), "compile, worktree:w1"));
    eprintln!("worktree ledger hook time: {without:.2} ms without a declared worktree, {with:.2} ms with one (median of 15, 4 open rows, three repositories)");
}
