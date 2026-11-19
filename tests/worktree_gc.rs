//! Worktree gc (`src/worktree_gc.rs`, `src/worktree_gc_docker.rs`) against
//! temp git repositories, a temp bare remote, temp stores and a fake Docker:
//! nothing here touches a real worktree, repository, volume or container,
//! and nothing calls the real docker or gh.

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use atlas::lanes::{Class, Holder, Item, Source};
use atlas::orchestrate::Kind;
use atlas::project::Repo;
use atlas::worktree_gc::{self as gc, Act, Context, Dirty, GcSettings, PrEnd, Probe, Work};
use atlas::worktree_gc_docker::{self as dk, Docker};
use atlas::worktree_ledger::Row;
use common::*;

const T: Duration = Duration::from_secs(30);

/// Runs git in `dir`; it must succeed.
fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().expect("run git");
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A repository `atlas` with one commit (`a.txt`, and a `.gitignore` that
/// ignores `target/` and `*.log`) pushed to a temp bare remote `origin`, and
/// its worktrees folder.
struct Fx {
    _t: Temp,
    root: PathBuf,
    repo: PathBuf,
    remote: PathBuf,
    globs: Vec<String>,
}

impl Fx {
    fn new(tag: &str) -> Fx {
        let t = Temp::new(tag);
        let root = t.path().to_path_buf();
        let repo = root.join("atlas");
        let remote = root.join("remote.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(&root, &["init", "-q", "--bare", "-b", "main", &remote.to_string_lossy()]);
        git(&repo, &["init", "-q", "-b", "main"]);
        for (k, v) in [("user.name", "atlas-test"), ("user.email", "atlas-test@example.invalid"), ("commit.gpgsign", "false")] {
            git(&repo, &["config", k, v]);
        }
        git(&repo, &["config", "core.hooksPath", &root.join("no-hooks").to_string_lossy()]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n*.log\n").unwrap();
        git(&repo, &["add", "-A"]);
        // Made before every ledger row a test writes (2022): a worktree from
        // it has no commits of its own.
        let o = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "-q", "-m", "init"])
            .env("GIT_AUTHOR_DATE", "2021-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2021-01-01T00:00:00Z")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        git(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
        git(&repo, &["push", "-q", "origin", "main"]);
        git(&repo, &["fetch", "-q", "origin"]);
        let globs = GcSettings::default().disposable;
        Fx { _t: t, root, repo, remote, globs }
    }

    /// A worktree with no commits of its own: `git worktree add` at
    /// `wts/NAME` on a new branch NAME from origin/main.
    fn add_new(&self, name: &str) -> PathBuf {
        let p = self.root.join("wts").join(name);
        git(&self.repo, &["worktree", "add", "-q", "--no-track", "-b", name, &p.to_string_lossy(), "origin/main"]);
        p
    }

    /// One commit of `wt`'s own (`own-NAME.txt`) landed on origin/main: its
    /// work merged.
    fn land(&self, wt: &Path) {
        let name = wt.file_name().unwrap().to_string_lossy().into_owned();
        // From origin/main's tip, so the push is a fast-forward (no rebase:
        // it leaves AUTO_MERGE behind).
        git(&self.repo, &["fetch", "-q", "origin"]);
        git(wt, &["reset", "-q", "--keep", "origin/main"]);
        let n = git(wt, &["rev-list", "--count", "HEAD"]);
        std::fs::write(wt.join(format!("own-{name}.txt")), format!("mine {}\n", n.trim())).unwrap();
        git(wt, &["add", "-A"]);
        git(wt, &["commit", "-q", "-m", &format!("work of {name}")]);
        git(wt, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
        git(&self.repo, &["fetch", "-q", "origin"]);
    }

    /// A worktree whose own work is merged ([`Fx::add_new`], [`Fx::land`]).
    fn add(&self, name: &str) -> PathBuf {
        let p = self.add_new(name);
        self.land(&p);
        p
    }

    fn repos(&self) -> Vec<Repo> {
        vec![Repo { name: "atlas".into(), path: Some(self.repo.clone()), ..Repo::default() }]
    }

    fn probe<'a>(&'a self, wt: &'a Path) -> Probe<'a> {
        Probe { path: wt, repo: Some(&self.repo), timeout: T, build: &[], disposable: &self.globs }
    }

    /// The classes of work `wt` holds.
    fn classes(&self, wt: &Path) -> Vec<Work> {
        gc::wip(&self.probe(wt)).0.into_iter().map(|(w, _)| w).collect()
    }

    /// `wt` holds exactly `want`, and gc's removal leaves it in place.
    fn kept_for(&self, wt: &Path, want: &[Work]) {
        let got = self.classes(wt);
        assert_eq!(got, want, "classes of {}", wt.display());
        let r = gc::remove_tree(&self.repo, wt, &[], &self.globs, T);
        assert!(r.is_err(), "removed a worktree that holds {want:?}");
        assert!(wt.is_dir(), "the folder went");
    }
}

fn row(id: i64, path: &Path, lane: &str, branch: &str) -> Row {
    Row {
        id,
        path: path.to_string_lossy().into_owned(),
        path_key: atlas::worktree_ledger::path_key(&path.to_string_lossy()),
        repo: "atlas".into(),
        branch: branch.into(),
        lane: lane.into(),
        description: "d".into(),
        tool_use_id: "t".into(),
        session: "s".into(),
        // After the fixture's first commit (2021) and before every commit a
        // test makes: those are the worktree's own.
        created_at: "2022-01-01T00:00:00Z".into(),
        last_modified: "2022-01-01T00:00:00Z".into(),
        removed_at: None,
        ended_at: None,
    }
}

/// The settings with no quiet period (each test's trees are seconds old).
/// `RunLock::take` once the last holder is gone. A flock goes with the open
/// file, and a test thread that starts a child process lends that child every
/// open file until it execs, so a lock this test dropped can outlive the drop
/// for a moment.
fn take_after_release(p: &Path, now: i64) -> Result<gc::RunLock, String> {
    let mut last = Err(String::new());
    for _ in 0..50 {
        last = gc::RunLock::take(p, now);
        if last.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    last
}

fn settings(dirty: Dirty) -> GcSettings {
    GcSettings { dirty, quiet: Duration::ZERO, ..GcSettings::default() }
}

#[test]
fn a_fresh_worktree_holds_no_work() {
    let fx = Fx::new("gc-clean");
    let wt = fx.add("clean");
    // Its ignored build folder is not work.
    std::fs::create_dir_all(wt.join("target").join("debug")).unwrap();
    std::fs::write(wt.join("target").join("debug").join("bin"), "b").unwrap();
    assert_eq!(fx.classes(&wt), Vec::<Work>::new());
}

#[test]
fn an_ignored_file_outside_the_build_folders_is_work() {
    let fx = Fx::new("gc-ignored");
    let wt = fx.add("ign");
    std::fs::create_dir_all(wt.join("target").join("debug")).unwrap();
    std::fs::write(wt.join("target").join("debug").join("bin"), "b").unwrap();
    std::fs::write(wt.join("x.log"), "log").unwrap();
    fx.kept_for(&wt, &[Work::Ignored]);
    let (found, _) = gc::wip(&fx.probe(&wt));
    assert!(found[0].1.contains("x.log") && !found[0].1.contains("target"), "{found:?}");
    // A configured build folder inside the tree is build output too.
    std::fs::remove_file(wt.join("x.log")).unwrap();
    std::fs::create_dir_all(wt.join("out")).unwrap();
    std::fs::write(wt.join("out").join("o.log"), "o").unwrap();
    assert_eq!(fx.classes(&wt), vec![Work::Ignored]);
    let build = [wt.join("out")];
    let (found, _) = gc::wip(&Probe { build: &build, ..fx.probe(&wt) });
    assert_eq!(found, Vec::new());
}

#[test]
fn wip_modified_file() {
    let fx = Fx::new("gc-mod");
    let wt = fx.add("m");
    std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
    fx.kept_for(&wt, &[Work::Modified]);
}

#[test]
fn wip_deleted_file() {
    let fx = Fx::new("gc-del");
    let wt = fx.add("d");
    std::fs::remove_file(wt.join("a.txt")).unwrap();
    fx.kept_for(&wt, &[Work::Deleted]);
}

#[test]
fn wip_staged_change() {
    let fx = Fx::new("gc-staged");
    let wt = fx.add("s");
    std::fs::write(wt.join("new.txt"), "n\n").unwrap();
    git(&wt, &["add", "new.txt"]);
    fx.kept_for(&wt, &[Work::Staged]);
}

#[test]
fn wip_untracked_file_not_ignored() {
    let fx = Fx::new("gc-untracked");
    let wt = fx.add("u");
    std::fs::create_dir_all(wt.join("deep").join("er")).unwrap();
    std::fs::write(wt.join("deep").join("er").join("notes.md"), "mine\n").unwrap();
    fx.kept_for(&wt, &[Work::Untracked]);
}

#[test]
fn wip_unpushed_commit() {
    let fx = Fx::new("gc-unpushed");
    let wt = fx.add("p");
    std::fs::write(wt.join("a.txt"), "committed\n").unwrap();
    git(&wt, &["commit", "-q", "-am", "local only"]);
    fx.kept_for(&wt, &[Work::Unpushed]);
}

#[test]
fn wip_branch_in_a_repository_with_no_remote_counts_every_commit() {
    let fx = Fx::new("gc-noremote");
    git(&fx.repo, &["remote", "remove", "origin"]);
    let wt = fx.root.join("wts").join("n");
    git(&fx.repo, &["worktree", "add", "-q", "-b", "n", &wt.to_string_lossy(), "main"]);
    fx.kept_for(&wt, &[Work::Unpushed]);
}

#[test]
fn wip_operation_in_progress() {
    let fx = Fx::new("gc-bisect");
    let wt = fx.add("b");
    git(&wt, &["bisect", "start"]);
    fx.kept_for(&wt, &[Work::InProgress]);
}

#[test]
fn wip_rebase_state_folder_counts() {
    let fx = Fx::new("gc-rebase");
    let wt = fx.add("r");
    let own = git(&wt, &["rev-parse", "--path-format=absolute", "--git-dir"]);
    std::fs::create_dir_all(Path::new(own.trim()).join("rebase-merge")).unwrap();
    fx.kept_for(&wt, &[Work::InProgress]);
}

#[test]
fn wip_locked_worktree() {
    let fx = Fx::new("gc-locked");
    let wt = fx.add("l");
    git(&fx.repo, &["worktree", "lock", "--reason", "on a stick", &wt.to_string_lossy()]);
    fx.kept_for(&wt, &[Work::Locked]);
}

#[test]
fn wip_stash_on_its_branch() {
    let fx = Fx::new("gc-stash");
    let wt = fx.add("st");
    std::fs::write(wt.join("a.txt"), "stashed\n").unwrap();
    git(&wt, &["stash", "-q"]);
    fx.kept_for(&wt, &[Work::Stash]);
    // A stash on another branch is not this worktree's.
    let other = fx.add("other");
    assert_eq!(fx.classes(&other), Vec::<Work>::new());
}

#[test]
fn wip_git_failing_or_timing_out_counts_as_work() {
    let fx = Fx::new("gc-timeout");
    let wt = fx.add("t");
    let (found, _) = gc::wip(&Probe { timeout: Duration::ZERO, ..fx.probe(&wt) });
    assert!(found.iter().any(|(w, _)| *w == Work::Unknown), "{found:?}");
    assert!(gc::remove_tree(&fx.repo, &wt, &[], &fx.globs, Duration::ZERO).is_err());
    assert!(wt.is_dir());
    // A worktree whose own git folder is broken: git fails on it.
    let wt2 = fx.add("t2");
    std::fs::write(wt2.join(".git"), "gitdir: nowhere\n").unwrap();
    let got = fx.classes(&wt2);
    assert!(got.contains(&Work::Unknown) || got.contains(&Work::NotAWorktree), "{got:?}");
    assert!(gc::remove_tree(&fx.repo, &wt2, &[], &fx.globs, T).is_err());
    assert!(wt2.is_dir());
}

#[test]
fn wip_path_not_a_worktree_of_the_ledger_repository() {
    let fx = Fx::new("gc-notwt");
    let other = Fx::new("gc-notwt-other");
    let wt = other.add("theirs");
    // A clean worktree, but of another repository than the one named.
    assert_eq!(other.classes(&wt), Vec::<Work>::new());
    fx.kept_for(&wt, &[Work::NotAWorktree]);
    // The main checkout is never one.
    fx.kept_for(&fx.repo.clone(), &[Work::NotAWorktree]);
    // A repository the configuration does not name.
    let (found, _) = gc::wip(&Probe { repo: None, ..fx.probe(&wt) });
    assert_eq!(found.iter().map(|(w, _)| *w).collect::<Vec<_>>(), vec![Work::NotAWorktree]);
}

/// A plan over `rows` with nothing running or waiting.
fn plan_of(fx: &Fx, rows: &[Row], s: &GcSettings, ends: &[PrEnd], holders: Option<&[Holder]>, waiting: &[Item]) -> Vec<gc::Entry> {
    let repos = fx.repos();
    let cx = Context { rows, repos: &repos, holders, waiting, pr_ends: ends, settings: s };
    gc::plan(&cx, true)
}

fn apply_all(fx: &Fx, rows: &[Row], s: &GcSettings, entries: &mut [gc::Entry]) -> Vec<String> {
    let repos = fx.repos();
    let cx = Context { rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: s };
    gc::apply(&cx, entries, &|| Some(Vec::new()))
}

#[test]
fn stale_and_clean_is_removed_without_force_and_the_dry_run_lists_it() {
    let fx = Fx::new("gc-stale");
    let wt = fx.add("done");
    std::fs::create_dir_all(wt.join("target")).unwrap();
    std::fs::write(wt.join("target").join("big"), vec![0u8; 4096]).unwrap();
    let tdir = fx.root.join("targets").join("done");
    std::fs::create_dir_all(&tdir).unwrap();
    std::fs::write(tdir.join("o"), vec![0u8; 8192]).unwrap();
    let s =
        GcSettings { target_dirs: vec![fx.root.join("targets").join("{name}").to_string_lossy().into_owned()], ..settings(Dirty::Report) };
    let rows = [row(1, &wt, "LANE-A", "done")];
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Remove, "{:?}", e[0]);
    assert!(e[0].why.contains("merged"), "{}", e[0].why);
    // The dry run: the plan with sizes, nothing touched.
    let text = gc::report(&e, None, false);
    assert!(text.contains("dry run") && text.starts_with("atlas worktrees gc") && text.contains("remove "), "{text}");
    assert!(text.contains("KB") && text.contains("reclaimable"), "{text}");
    assert!(gc::reclaimable(&e) >= 4096 + 8192);
    assert!(wt.is_dir() && tdir.is_dir());
    // Applied: the tree, then its linked build folder; the branch stays.
    let removed = apply_all(&fx, &rows, &s, &mut e);
    assert_eq!(removed.len(), 1);
    assert!(!wt.exists() && !tdir.exists(), "{:?}", e[0].done);
    assert!(git(&fx.repo, &["branch", "--list", "done"]).contains("done"));
}

#[test]
fn a_build_folder_another_ledger_worktree_links_is_never_deleted() {
    let fx = Fx::new("gc-shared");
    let a = fx.add("same");
    let other_root = fx.root.join("elsewhere");
    std::fs::create_dir_all(&other_root).unwrap();
    let b = other_root.join("same");
    git(&fx.repo, &["worktree", "add", "-q", "-b", "same2", &b.to_string_lossy(), "main"]);
    let t = fx.root.join("targets").join("same");
    std::fs::create_dir_all(&t).unwrap();
    let s =
        GcSettings { target_dirs: vec![fx.root.join("targets").join("{name}").to_string_lossy().into_owned()], ..settings(Dirty::Report) };
    let rows = [row(1, &a, "A", "same"), row(2, &b, "B", "same2")];
    let repos = fx.repos();
    let cx = Context { rows: &rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: &s };
    assert_eq!(gc::linked_target(&cx, &rows[0]), None);
    // A template without {name} links nothing.
    assert_eq!(gc::target_of("H:/t", "x"), None);
}

#[test]
fn not_merged_is_kept_and_a_merged_pr_event_makes_it_stale() {
    let fx = Fx::new("gc-notmerged");
    let wt = fx.add("feat");
    std::fs::write(wt.join("a.txt"), "feature\n").unwrap();
    git(&wt, &["commit", "-q", "-am", "feature"]);
    git(&wt, &["push", "-q", "origin", "feat"]);
    let rows = [row(1, &wt, "L", "feat")];
    let s = settings(Dirty::Report);
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!((e[0].act, e[0].why.as_str()), (Act::Keep, "not merged"));
    let head = git(&wt, &["rev-parse", "HEAD"]).trim().to_string();
    let ends = [PrEnd { pr: 7, verdict: "merged".into(), head: head.clone() }];
    let e = plan_of(&fx, &rows, &s, &ends, Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Remove, "{:?}", e[0]);
    assert!(e[0].why.contains("PR #7 merged"));
    // Another head's event does not count.
    let ends = [PrEnd { pr: 8, verdict: "closed".into(), head: "0123456789abcdef".into() }];
    assert_eq!(plan_of(&fx, &rows, &s, &ends, Some(&[]), &[])[0].act, Act::Keep);
}

fn class(lane: Option<&str>, uses: &[&str]) -> Class {
    Class {
        lane: lane.map(str::to_string),
        lane_src: Source::Declared,
        priority: atlas::lanes::Priority::default(),
        priority_src: Source::Declared,
        uses: uses.iter().map(|u| (u.to_string(), Source::Declared)).collect(),
        read_only: false,
        repo: None,
        branch: None,
    }
}

#[test]
fn a_live_agent_or_a_waiting_item_or_an_unreadable_view_keeps_it() {
    let fx = Fx::new("gc-live");
    let wt = fx.add("busy");
    let rows = [row(1, &wt, "LANE-B", "busy")];
    let s = settings(Dirty::Report);
    let by_lane = [Holder { name: "agent-1".into(), kind: Kind::Builder, class: class(Some("LANE-B"), &[]) }];
    let e = plan_of(&fx, &rows, &s, &[], Some(&by_lane), &[]);
    assert_eq!(e[0].act, Act::Keep);
    assert!(e[0].why.contains("live agent agent-1"), "{}", e[0].why);
    let by_name = [Holder { name: "agent-2".into(), kind: Kind::Builder, class: class(Some("OTHER"), &["worktree:BUSY"]) }];
    assert_eq!(plan_of(&fx, &rows, &s, &[], Some(&by_name), &[])[0].act, Act::Keep);
    let item = Item {
        key: "queue:x".into(),
        kind: Kind::Builder,
        class: class(None, &["worktree:busy"]),
        waited_minutes: 0,
        order: None,
        takes_slot: true,
        unmet: Vec::new(),
    };
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), std::slice::from_ref(&item));
    assert!(e[0].why.contains("waiting queue item queue:x"), "{}", e[0].why);
    assert_eq!(plan_of(&fx, &rows, &s, &[], None, &[])[0].act, Act::Keep);
    // Nothing holds it: stale.
    let other = [Holder { name: "agent-3".into(), kind: Kind::Builder, class: class(Some("ELSE"), &["worktree:else"]) }];
    assert_eq!(plan_of(&fx, &rows, &s, &[], Some(&other), &[])[0].act, Act::Remove);
}

#[test]
fn work_that_appears_after_the_plan_stops_the_removal() {
    let fx = Fx::new("gc-race");
    let wt = fx.add("race");
    let rows = [row(1, &wt, "L", "race")];
    let s = settings(Dirty::Report);
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Remove);
    std::fs::write(wt.join("late.txt"), "written after the plan\n").unwrap();
    assert!(apply_all(&fx, &rows, &s, &mut e).is_empty());
    assert!(wt.join("late.txt").is_file());
    assert!(e[0].done.as_deref().unwrap_or("").contains("holds work now"), "{:?}", e[0].done);
}

#[test]
fn dirty_report_changes_nothing_and_lists_the_work() {
    let fx = Fx::new("gc-report");
    let wt = fx.add("rep");
    std::fs::write(wt.join("mine.txt"), "x\n").unwrap();
    std::fs::create_dir_all(wt.join("target")).unwrap();
    let rows = [row(1, &wt, "L", "rep")];
    let s = settings(Dirty::Report);
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Work);
    assert!(apply_all(&fx, &rows, &s, &mut e).is_empty());
    assert!(wt.join("mine.txt").is_file() && wt.join("target").is_dir());
    let text = gc::report(&e, None, true);
    assert!(text.contains("work ") && text.contains("untracked: 1 untracked file"), "{text}");
}

#[test]
fn dirty_remove_target_deletes_only_build_output() {
    let fx = Fx::new("gc-rt");
    let wt = fx.add("rt");
    std::fs::write(wt.join("mine.txt"), "x\n").unwrap();
    std::fs::create_dir_all(wt.join("target").join("debug")).unwrap();
    let linked = fx.root.join("targets").join("rt");
    std::fs::create_dir_all(&linked).unwrap();
    let s = GcSettings {
        target_dirs: vec![fx.root.join("targets").join("{name}").to_string_lossy().into_owned()],
        ..settings(Dirty::RemoveTarget)
    };
    let rows = [row(1, &wt, "L", "rt")];
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Target);
    assert!(apply_all(&fx, &rows, &s, &mut e).is_empty());
    assert!(!wt.join("target").exists() && !linked.exists(), "{:?}", e[0].done);
    assert!(wt.join("mine.txt").is_file() && wt.join("a.txt").is_file());
    assert_eq!(fx.classes(&wt), vec![Work::Untracked]);
    // A worktree git cannot answer for keeps its build output too.
    let broken = fx.add("broken");
    std::fs::create_dir_all(broken.join("target")).unwrap();
    std::fs::write(broken.join(".git"), "gitdir: nowhere\n").unwrap();
    let rows = [row(2, &broken, "L", "broken")];
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    if e[0].act != Act::Keep {
        assert_eq!(e[0].act, Act::Work, "{:?}", e[0]);
    }
    apply_all(&fx, &rows, &s, &mut e);
    assert!(broken.join("target").is_dir());
}

#[test]
fn dirty_commit_delete_pushes_verifies_then_removes() {
    let fx = Fx::new("gc-cd");
    let wt = fx.add("cd");
    std::fs::write(wt.join("a.txt"), "edited\n").unwrap();
    std::fs::write(wt.join("fresh.txt"), "new work\n").unwrap();
    std::fs::write(wt.join("skip.log"), "ignored\n").unwrap();
    let rows = [row(1, &wt, "L", "cd")];
    let s = settings(Dirty::CommitDelete);
    // An ignored file outside the build folders: a commit cannot save it.
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Work, "{:?}", e[0]);
    std::fs::create_dir_all(wt.join("target")).unwrap();
    std::fs::rename(wt.join("skip.log"), wt.join("target").join("skip.log")).unwrap();
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Commit, "{:?}", e[0]);
    let removed = apply_all(&fx, &rows, &s, &mut e);
    assert_eq!(removed.len(), 1, "{:?}", e[0].done);
    assert!(!wt.exists());
    // The work is on the remote: the untracked file and the edit.
    assert_eq!(git(&fx.remote, &["show", "cd:fresh.txt"]), "new work\n");
    assert_eq!(git(&fx.remote, &["show", "cd:a.txt"]), "edited\n");
    assert!(git(&fx.remote, &["log", "-1", "--format=%s", "cd"]).starts_with("WIP: saved by atlas worktrees gc"));
    let r = Command::new("git").arg("-C").arg(&fx.remote).args(["show", "cd:target/skip.log"]).output().unwrap();
    assert!(!r.status.success(), "an ignored file was committed");
}

#[test]
fn dirty_commit_delete_with_a_failing_push_leaves_everything() {
    let fx = Fx::new("gc-cdfail");
    let wt = fx.add("cf");
    std::fs::write(wt.join("fresh.txt"), "new work\n").unwrap();
    git(&fx.repo, &["remote", "set-url", "origin", &fx.root.join("no-such-remote.git").to_string_lossy()]);
    let rows = [row(1, &wt, "L", "cf")];
    let s = settings(Dirty::CommitDelete);
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Commit);
    assert!(apply_all(&fx, &rows, &s, &mut e).is_empty());
    assert!(e[0].done.as_deref().unwrap_or("").contains("push failed"), "{:?}", e[0].done);
    assert!(wt.join("fresh.txt").is_file());
    // The work is still there, now as a local commit: still work.
    assert_eq!(fx.classes(&wt), vec![Work::Unpushed]);
}

#[test]
fn dirty_commit_delete_never_acts_on_work_a_commit_cannot_save() {
    let fx = Fx::new("gc-cdlock");
    let wt = fx.add("cl");
    std::fs::write(wt.join("fresh.txt"), "x\n").unwrap();
    git(&fx.repo, &["worktree", "lock", &wt.to_string_lossy()]);
    let rows = [row(1, &wt, "L", "cl")];
    let s = settings(Dirty::CommitDelete);
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Work);
    assert!(apply_all(&fx, &rows, &s, &mut e).is_empty());
    assert!(wt.join("fresh.txt").is_file());
    assert_eq!(git(&wt, &["rev-list", "--count", "HEAD", "--not", "--remotes"]).trim(), "0");
    // Detached HEAD: no branch to push to.
    let d = fx.add("det");
    git(&d, &["checkout", "-q", "--detach"]);
    std::fs::write(d.join("fresh.txt"), "x\n").unwrap();
    assert!(gc::commit_delete(&fx.repo, &d, "", &[], &s).unwrap_err().contains("detached"));
    assert!(d.join("fresh.txt").is_file());
}

/// A fake docker: canned answers by the first two words, every call kept.
struct FakeDocker {
    ps: Option<String>,
    inspect: String,
    volumes: String,
    calls: RefCell<Vec<String>>,
}

impl Docker for FakeDocker {
    fn run(&self, args: &[&str]) -> Result<String, String> {
        self.calls.borrow_mut().push(args.join(" "));
        match args {
            ["ps", ..] => self.ps.clone().ok_or_else(|| "docker: not running".to_string()),
            ["inspect", ..] => Ok(self.inspect.clone()),
            ["volume", "ls", ..] => Ok(self.volumes.clone()),
            // Only atlas's own objects may go: anything else is a bug.
            ["rm", n] | ["volume", "rm", n] if n.starts_with("atlas-linux-") => Ok(String::new()),
            ["rm", ..] | ["volume", "rm", ..] => Err(format!("refused: docker {args:?} is not an atlas object")),
            _ => Err(format!("unexpected docker {args:?}")),
        }
    }
}

const NOW: i64 = 1_800_000_000;

fn at(secs_ago: i64) -> String {
    // 1_800_000_000 is 2027-01-15T08:00:00Z.
    let t = NOW - secs_ago;
    let (d, s) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Civil from days (H. Hinnant).
    let z = d + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.123456789Z", s / 3600, s % 3600 / 60, s % 60)
}

#[test]
fn docker_removes_only_atlas_objects_of_gone_checkouts_and_reports_the_rest() {
    let gone_path = "D:/src/wts/gone-one";
    let live_path = "D:/src/wts/live-one";
    let g = dk::checkout_hash(&atlas::worktree_ledger::path_key(gone_path), cfg!(windows));
    let l = dk::checkout_hash(&atlas::worktree_ledger::path_key(live_path), cfg!(windows));
    let r = "deadbeef0123"; // gone, but its container runs
    let inspect = [
        format!("/atlas-linux-test-{g}\texited\t{}\tatlas-linux-target-{g},", at(3 * 86_400)),
        format!("/atlas-linux-test-{l}\texited\t{}\tatlas-linux-target-{l},", at(3_600)),
        format!("/atlas-linux-test-{r}\trunning\t0001-01-01T00:00:00Z\tatlas-linux-target-{r},"),
        format!("/postgres\texited\t{}\tpgdata,", at(5 * 7 * 86_400)),
        "/fresh\tcreated\t0001-01-01T00:00:00Z\t".to_string(),
    ]
    .join("\n");
    let volumes = [
        format!("atlas-linux-target-{g}"),
        format!("atlas-linux-target-{l}"),
        format!("atlas-linux-target-{r}"),
        "pgdata".into(),
        "3f2a9c".into(),
        "atlas-linux-cargo-home".into(),
    ]
    .join("\n");
    let d = FakeDocker { ps: Some("id1\nid2\nid3\nid4\nid5\n".into()), inspect, volumes, calls: RefCell::new(Vec::new()) };
    let (containers, vols) = dk::read(&d).expect("docker read");
    let mut gone = std::collections::BTreeMap::new();
    gone.insert(g.clone(), gone_path.to_string());
    gone.insert(r.to_string(), "H:/x/running".to_string());
    gone.insert(l.clone(), live_path.to_string());
    let live: std::collections::BTreeSet<String> = [l.clone()].into_iter().collect();
    let p = dk::plan(&containers, &vols, &gone, &live, NOW);
    assert_eq!(p.remove_containers, vec![format!("atlas-linux-test-{g}")]);
    assert_eq!(p.remove_volumes, vec![format!("atlas-linux-target-{g}")]);
    let text = p.report.join("\n");
    assert!(text.contains("docker container postgres: exited 5 weeks ago"), "{text}");
    assert!(text.contains("docker container fresh: created, never ran"), "{text}");
    assert!(text.contains("docker volume pgdata: used by postgres (exited 5 weeks ago)"), "{text}");
    assert!(text.contains("docker volume 3f2a9c: unreferenced, last activity unknown"), "{text}");
    assert!(text.contains(&format!("atlas-linux-test-{r}: running; kept while it runs")), "{text}");
    assert!(text.contains(&format!("docker volume atlas-linux-target-{r}: kept, used by")), "{text}");
    assert!(text.contains("atlas-linux-cargo-home: unreferenced"), "{text}");
    assert!(dk::apply(&d, &p).is_empty());
    let calls = d.calls.borrow();
    let destructive: Vec<&String> = calls.iter().filter(|c| c.starts_with("rm") || c.starts_with("volume rm")).collect();
    assert_eq!(destructive, vec![&format!("rm atlas-linux-test-{g}"), &format!("volume rm atlas-linux-target-{g}")]);
}

#[test]
fn docker_missing_or_not_running_is_a_quiet_skip() {
    let d = FakeDocker { ps: None, inspect: String::new(), volumes: String::new(), calls: RefCell::new(Vec::new()) };
    assert!(dk::read(&d).is_none());
    let none = FakeDocker { ps: Some(String::new()), inspect: String::new(), volumes: String::new(), calls: RefCell::new(Vec::new()) };
    let (c, v) = dk::read(&none).unwrap();
    assert!(c.is_empty() && v.is_empty());
    // No containers: no inspect call with no ids.
    assert!(!none.calls.borrow().iter().any(|c| c.starts_with("inspect")));
    assert!(dk::DISK_NOTE.contains("does not shrink by itself"));
}

#[test]
fn the_checkout_hash_matches_xtask_linux_test() {
    // xtask/src/linux_test.rs: sha256 of the path, `/` and lowercase on Windows.
    assert_eq!(dk::checkout_hash("C:\\Work\\X", true), dk::checkout_hash("c:/work/x", true));
    assert_eq!(dk::checkout_hash("/a/B", false), atlas::sha256::hex(b"/a/B")[..12].to_string());
}

#[test]
fn the_hook_runs_hourly_or_at_once_after_a_merge_and_tells_the_threshold() {
    assert!(gc::due("", NOW, "SessionStart", false));
    assert!(gc::due(&format!("{} 5", NOW - 3_600), NOW, "SubagentStop", false));
    assert!(!gc::due(&format!("{} 5", NOW - 60), NOW, "SessionStart", false));
    assert!(!gc::due(&format!("{} 5", NOW - 7_200), NOW, "UserPromptSubmit", false));
    assert!(gc::due(&format!("{} 5", NOW - 60), NOW, "UserPromptSubmit", true));
    assert_eq!(gc::stamp_rid(&format!("{NOW} 42\n")), 42);
    let off = GcSettings { notify_bytes: 10 << 30, ..GcSettings::default() };
    let l = gc::line(&format!("{} 3\n", 12u64 << 30), &off).expect("over the threshold");
    assert!(l.contains("3 stale worktrees hold 12.0 GB") && l.contains("auto_gc is off"), "{l}");
    assert_eq!(gc::line(&format!("{} 3\n", 9u64 << 30), &off), None);
    assert_eq!(gc::line("", &off), None);
    let on = GcSettings { auto_gc: true, ..off };
    assert_eq!(gc::line(&format!("{} 3\n", 12u64 << 30), &on), None);
}

#[test]
fn the_config_section_parses_and_refuses_bad_values() {
    let base = Path::new("/base");
    let c = atlas::config::Config::parse(
        r#"{"worktrees": {"auto_gc": true, "dirty": "commit_delete", "notify_gb": 3, "target_dirs": ["t/{name}"]}}"#,
        base,
    )
    .unwrap();
    let s = GcSettings::from_spec(c.worktrees.as_ref());
    assert!(s.auto_gc);
    assert_eq!(s.dirty, Dirty::CommitDelete);
    assert_eq!(s.notify_bytes, 3 << 30);
    assert!(s.target_dirs[0].ends_with("{name}") && Path::new(&s.target_dirs[0]).is_absolute() == base.join("t").is_absolute());
    let d = GcSettings::from_spec(None);
    assert_eq!((d.auto_gc, d.dirty), (false, Dirty::Report));
    assert!(atlas::config::Config::parse(r#"{"worktrees": {"dirty": "nuke"}}"#, base).is_err());
    assert!(atlas::config::Config::parse(r#"{"worktrees": {"target_dirs": ["H:/targets"]}}"#, base).is_err());
    assert!(atlas::config::Config::parse(r#"{"worktrees": {"autogc": true}}"#, base).is_err());
}

#[test]
fn run_gc_removes_a_ledger_worktree_stamps_the_ledger_and_its_docker_objects() {
    let fx = Fx::new("gc-run");
    let wt = fx.add("finished");
    let keep = fx.add("ongoing");
    std::fs::write(keep.join("mine.txt"), "x\n").unwrap();
    let store = atlas::memory::MemoryStore::open(&fx.root.join("state").join("memory.sqlite")).unwrap();
    let (all, errs) = atlas::worktree_ledger::worktrees(&fx.repos(), &[]);
    assert!(errs.is_empty());
    let o = atlas::worktree_ledger::Owner { lane: Some("GC-LANE"), holds: true, description: "d", tool_use_id: "t", session: "s" };
    for w in &all {
        // Recorded before the worktrees' own commits.
        atlas::worktree_ledger::record(store.conn(), store.path(), w, &o, "2022-01-01T00:00:00Z").unwrap();
    }
    let h = dk::checkout_hash(
        &atlas::worktree_ledger::path_key(&all.iter().find(|w| w.path.ends_with("finished")).unwrap().path),
        cfg!(windows),
    );
    let d = FakeDocker {
        ps: Some("id1\n".into()),
        inspect: format!("/atlas-linux-test-{h}\texited\t{}\tatlas-linux-target-{h},\n", at(86_400)),
        volumes: format!("atlas-linux-target-{h}\n"),
        calls: RefCell::new(Vec::new()),
    };
    let orch = atlas::orchestrate::resolve(None, None, None, |_| None);
    let s = settings(Dirty::Report);
    let dry = gc::run_gc(&store, &orch, &fx.repos(), &s, Some(&d), false, None).unwrap();
    assert!(dry.text.contains("dry run") && wt.is_dir(), "{}", dry.text);
    assert_eq!(dry.stale, 1);
    let o = gc::run_gc(&store, &orch, &fx.repos(), &s, Some(&d), true, None).unwrap();
    assert!(!wt.exists() && keep.join("mine.txt").is_file(), "{}", o.text);
    assert_eq!(o.stale, 0);
    let open: Vec<String> = atlas::worktree_ledger::open_rows(store.conn()).unwrap().into_iter().map(|r| r.path).collect();
    assert!(open.iter().all(|p| !p.ends_with("finished")), "{open:?}");
    let calls = d.calls.borrow();
    assert!(
        calls.contains(&format!("rm atlas-linux-test-{h}")) && calls.contains(&format!("volume rm atlas-linux-target-{h}")),
        "{calls:?}"
    );
    assert!(o.text.contains("does not shrink by itself"));
}

/// `r` with ledger times long past the quiet period.
fn old(r: Row) -> Row {
    Row { created_at: "2022-01-01T00:00:00Z".into(), last_modified: "2022-01-01T00:00:00Z".into(), ..r }
}

#[test]
fn a_worktree_at_the_default_tip_is_new_not_merged() {
    let fx = Fx::new("gc-tip");
    let p = fx.add_new("tip");
    let rows = [row(1, &p, "L", "tip")];
    let s = settings(Dirty::Report);
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Keep, "{:?}", e[0]);
    assert!(e[0].why.starts_with("new"), "{}", e[0].why);
    // The default branch moves on past it: still new, with no commits of
    // its own, however far main goes.
    let other = fx.add("other");
    fx.land(&other);
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Keep, "{:?}", e[0]);
    assert!(e[0].why.starts_with("new: no commits of its own"), "{}", e[0].why);
    // A row made after HEAD's commit: HEAD is where it started, not its own.
    let after = [Row { created_at: "2099-01-01T00:00:00Z".into(), ..row(1, &other, "L", "other") }];
    let e = plan_of(&fx, &after, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Keep, "{:?}", e[0]);
    assert!(e[0].why.starts_with("new"), "{}", e[0].why);
    // Its own commit merged: done.
    fx.land(&p);
    fx.land(&other);
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Remove, "{:?}", e[0]);
    assert!(e[0].why.contains("merged"), "{}", e[0].why);
}

#[test]
fn run_gc_keeps_everything_when_a_session_of_the_running_agents_cannot_be_read() {
    let fx = Fx::new("gc-view");
    let wt = fx.add("finished");
    let store = atlas::memory::MemoryStore::open(&fx.root.join("state").join("memory.sqlite")).unwrap();
    let (all, _) = atlas::worktree_ledger::worktrees(&fx.repos(), &[]);
    let o = atlas::worktree_ledger::Owner { lane: Some("GC-LANE"), holds: true, description: "d", tool_use_id: "t", session: "s" };
    for w in &all {
        // Recorded before the worktrees' own commits.
        atlas::worktree_ledger::record(store.conn(), store.path(), w, &o, "2022-01-01T00:00:00Z").unwrap();
    }
    // A session whose agents folder cannot be listed: a file stands there.
    let tp = fx.root.join("sess.jsonl");
    let dir = atlas::orchestrate::subagents_dir(&tp.to_string_lossy());
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, "not a folder").unwrap();
    {
        let mut st = atlas::memory::MemoryStore::open(store.path()).unwrap();
        let mut w = st.writer().unwrap();
        let d = atlas::memory::Draft {
            title: "launch: job a1".into(),
            meta: format!("agent: a1\ntype: general-purpose\nsession: s1\ntranscript: {}\n", tp.to_string_lossy()),
            body: "Kind: builder\nLane: OTHER\n\nBuild it.".into(),
            reason: "seeding gc tests".into(),
            author: "hook".into(),
            client: "atlas-hook".into(),
            ..atlas::memory::Draft::default()
        };
        w.create("journal:OTHER:0001", "journal", atlas::hooks::LAUNCH_ENTRY, "OTHER", &d, None).unwrap();
        w.commit().unwrap();
    }
    let orch = atlas::orchestrate::resolve(None, None, None, |_| None);
    let s = settings(Dirty::Report);
    let dry = gc::run_gc(&store, &orch, &fx.repos(), &s, None, false, None).unwrap();
    assert_eq!(dry.stale, 0, "{}", dry.text);
    assert!(dry.text.contains("running agents could not be read"), "{}", dry.text);
    let o = gc::run_gc(&store, &orch, &fx.repos(), &s, None, true, None).unwrap();
    assert!(wt.is_dir(), "{}", o.text);
    // The folder readable (no agents in it): the view is whole again.
    std::fs::remove_file(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(gc::run_gc(&store, &orch, &fx.repos(), &s, None, false, None).unwrap().stale, 1);
}

#[test]
fn a_worktree_changed_within_the_quiet_period_is_in_use() {
    let fx = Fx::new("gc-quiet");
    let wt = fx.add("q");
    let quiet = GcSettings { quiet: Duration::from_secs(2 * 3600), ..settings(Dirty::Report) };
    assert_eq!(GcSettings::default().quiet, Duration::from_secs(2 * 3600));
    let now = atlas::memory::now();
    let young = [Row { created_at: now.clone(), last_modified: now, ..old(row(1, &wt, "L", "q")) }];
    let e = plan_of(&fx, &young, &quiet, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Keep);
    assert!(e[0].why.contains("ledger row changed within 120 min"), "{}", e[0].why);
    let rows = [old(row(1, &wt, "L", "q"))];
    let e = plan_of(&fx, &rows, &quiet, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Keep);
    assert!(e[0].why.contains("a file in it changed within 120 min"), "{}", e[0].why);
    assert_eq!(plan_of(&fx, &rows, &settings(Dirty::Report), &[], Some(&[]), &[])[0].act, Act::Remove);
}

#[test]
fn the_apply_loop_checks_live_agents_and_the_quiet_period_again() {
    let fx = Fx::new("gc-recheck");
    let wt = fx.add("rc");
    let rows = [old(row(1, &wt, "LANE-R", "rc"))];
    let s = settings(Dirty::Report);
    let repos = fx.repos();
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Remove);
    let cx = Context { rows: &rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: &s };
    // An agent of its lane started after the plan.
    let late = || Some(vec![Holder { name: "late".into(), kind: Kind::Builder, class: class(Some("LANE-R"), &[]) }]);
    assert!(gc::apply(&cx, &mut e, &late).is_empty());
    assert!(e[0].done.as_deref().unwrap_or("").contains("live agent late"), "{:?}", e[0].done);
    // The running agents cannot be read now.
    assert!(gc::apply(&cx, &mut e, &|| None).is_empty());
    assert!(e[0].done.as_deref().unwrap_or("").contains("could not be read"), "{:?}", e[0].done);
    // A change inside the quiet period.
    let quiet = GcSettings { quiet: Duration::from_secs(3600), ..settings(Dirty::Report) };
    let qcx = Context { settings: &quiet, ..cx };
    assert!(gc::apply(&qcx, &mut e, &|| Some(Vec::new())).is_empty());
    assert!(e[0].done.as_deref().unwrap_or("").contains("changed within 60 min"), "{:?}", e[0].done);
    assert!(wt.is_dir());
    assert_eq!(gc::apply(&cx, &mut e, &|| Some(Vec::new())).len(), 1);
    assert!(!wt.exists());
}

#[test]
fn a_second_run_while_one_holds_the_lock_does_nothing() {
    let fx = Fx::new("gc-lock");
    let wt = fx.add("locked-run");
    let store = atlas::memory::MemoryStore::open(&fx.root.join("state").join("memory.sqlite")).unwrap();
    let lp = gc::lock_path(store.path());
    let held = gc::RunLock::take(&lp, NOW).expect("the first run takes it");
    // Its file can be read (taken 60 s ago) or not (Windows locks reads).
    let busy = gc::RunLock::take(&lp, NOW + 60).unwrap_err();
    assert!(busy.contains("taken 60 s ago") || busy.contains("is held"), "{busy}");
    let orch = atlas::orchestrate::resolve(None, None, None, |_| None);
    let s = settings(Dirty::Report);
    for args in [["--apply".to_string()], ["--auto".to_string()]] {
        let out = gc::cli(&args, &store, &orch, &fx.repos(), &s).unwrap();
        assert!(out.contains("another run is under way") && out.contains("nothing done"), "{out}");
    }
    assert!(wt.is_dir() && !gc::summary_path(store.path()).exists());
    // Dropped: released, and the file stays (never deleted).
    drop(held);
    assert!(lp.is_file());
    // A file a run that died left (nothing locks it): taken.
    std::fs::write(&lp, format!("{} 1\n", NOW - 7_200)).unwrap();
    let again = take_after_release(&lp, NOW).expect("a dead run's lock file is taken");
    drop(again);
    let mine = take_after_release(&lp, NOW).unwrap();
    assert!(mine.token.starts_with(&format!("{NOW} {} ", std::process::id())), "{}", mine.token);
    let token = mine.token.clone();
    drop(mine);
    assert_eq!(std::fs::read_to_string(&lp).unwrap().trim(), token);
    // The stamp is written whole through a rename.
    let sp = fx.root.join("stamp");
    gc::write_atomic(&sp, "1 2\n").unwrap();
    assert_eq!(std::fs::read_to_string(&sp).unwrap(), "1 2\n");
}

#[test]
fn a_build_folder_that_is_a_checkout_is_never_deleted() {
    let fx = Fx::new("gc-typo");
    // `{root}/{name}` for `{root}/targets/{name}`: the worktree called
    // `atlas` maps to the main checkout.
    let wt = fx.root.join("wts").join("atlas");
    git(&fx.repo, &["worktree", "add", "-q", "-b", "atlas-wt", &wt.to_string_lossy(), "main"]);
    fx.land(&wt);
    let s = GcSettings { target_dirs: vec![fx.root.join("{name}").to_string_lossy().into_owned()], ..settings(Dirty::Report) };
    let rows = [old(row(1, &wt, "L", "atlas-wt"))];
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].target, None, "{:?}", e[0]);
    assert!(e[0].why.contains("target refused"), "{}", e[0].why);
    // The apply step checks again: a checkout handed to it is refused.
    e[0].target = Some((fx.repo.clone(), None));
    assert_eq!(apply_all(&fx, &rows, &s, &mut e).len(), 1);
    assert!(fx.repo.join("a.txt").is_file() && fx.repo.join(".git").exists(), "{:?}", e[0].done);
    assert!(e[0].done.as_deref().unwrap_or("").contains("target left: refused"), "{:?}", e[0].done);
    // A folder holding `.git`, or a folder above a checkout.
    let repos = fx.repos();
    let cx = Context { rows: &rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: &s };
    let plain = fx.root.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(gc::build_refusal(&cx, &plain), None);
    std::fs::write(plain.join(".git"), "gitdir: x\n").unwrap();
    assert!(gc::build_refusal(&cx, &plain).is_some());
    assert!(gc::build_refusal(&cx, &fx.root).is_some());
}

#[test]
fn commit_delete_refuses_large_or_secret_untracked_files() {
    let fx = Fx::new("gc-scan");
    let wt = fx.add("sc");
    let rows = [old(row(1, &wt, "L", "sc"))];
    let s = GcSettings { commit_limit: 1024, ..settings(Dirty::CommitDelete) };
    std::fs::write(wt.join("big.bin"), vec![7u8; 2048]).unwrap();
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Work, "{:?}", e[0]);
    assert!(e[0].why.contains("big.bin is 2.0 KB, over the 1.0 KB limit"), "{}", e[0].why);
    let err = gc::commit_delete(&fx.repo, &wt, "sc", &[], &s).unwrap_err();
    assert!(err.contains("not committed"), "{err}");
    std::fs::remove_file(wt.join("big.bin")).unwrap();
    std::fs::write(wt.join(".env"), "X=1\n").unwrap();
    assert!(gc::commit_delete(&fx.repo, &wt, "sc", &[], &s).unwrap_err().contains(".env has a denied name"));
    std::fs::remove_file(wt.join(".env")).unwrap();
    std::fs::write(wt.join("notes.txt"), "export API_TOKEN=notarealvalue123\n").unwrap();
    assert!(gc::commit_delete(&fx.repo, &wt, "sc", &[], &s).unwrap_err().contains("looks secret"));
    // Nothing was committed or pushed.
    assert!(git(&fx.remote, &["branch", "--list", "sc"]).trim().is_empty());
    assert!(wt.join("notes.txt").is_file());
    assert_eq!(fx.classes(&wt), vec![Work::Untracked]);
}

#[test]
fn intent_to_add_is_work_and_a_locked_tree_keeps_its_build_output() {
    let fx = Fx::new("gc-nits");
    let wt = fx.add("ita");
    std::fs::write(wt.join("later.txt"), "x\n").unwrap();
    git(&wt, &["add", "-N", "later.txt"]);
    fx.kept_for(&wt, &[Work::Staged]);
    let lk = fx.add("lk");
    std::fs::create_dir_all(lk.join("target")).unwrap();
    std::fs::write(lk.join("mine.txt"), "x\n").unwrap();
    git(&fx.repo, &["worktree", "lock", &lk.to_string_lossy()]);
    let rows = [old(row(1, &lk, "L", "lk"))];
    let s = settings(Dirty::RemoveTarget);
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Work, "{:?}", e[0]);
    apply_all(&fx, &rows, &s, &mut e);
    assert!(lk.join("target").is_dir());
}

#[test]
fn a_failed_commit_reports_the_index_lock_it_meets_and_leaves_it() {
    let fx = Fx::new("gc-ilock");
    let wt = fx.add("il");
    std::fs::write(wt.join("fresh.txt"), "x\n").unwrap();
    let lock = git(&wt, &["rev-parse", "--path-format=absolute", "--git-path", "index.lock"]);
    let lock = PathBuf::from(lock.trim());
    std::fs::write(&lock, "").unwrap();
    let s = settings(Dirty::CommitDelete);
    let err = gc::commit_delete(&fx.repo, &wt, "il", &[], &s).unwrap_err();
    assert!(err.contains("index.lock is left; atlas does not delete it"), "{err}");
    assert!(lock.is_file() && wt.join("fresh.txt").is_file());
    assert_eq!(gc::WRITE_TIMEOUT, Duration::from_secs(300));
}

#[test]
fn sizes_are_measured_only_as_far_as_the_cap() {
    let fx = Fx::new("gc-measure");
    let a = fx.add("ma");
    let b = fx.add("mb");
    let rows = [old(row(1, &a, "L", "ma")), old(row(2, &b, "L", "mb"))];
    let s = settings(Dirty::Report);
    let repos = fx.repos();
    let cx = Context { rows: &rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: &s };
    let mut e = gc::plan(&cx, false);
    assert!(e.iter().all(|e| e.act == Act::Remove && e.bytes.is_none()), "{e:?}");
    gc::measure(&mut e, Some(1));
    assert!(e[0].bytes.is_some_and(|n| n > 0) && e[1].bytes.is_none(), "{e:?}");
    gc::measure(&mut e, None);
    assert!(e[1].bytes.is_some());
}

/// Sets the modified time of `p` and everything under it to `secs` ago,
/// children before their folder.
fn age(p: &Path, secs: u64) {
    let md = std::fs::symlink_metadata(p).unwrap();
    if md.is_dir() {
        for e in std::fs::read_dir(p).unwrap() {
            age(&e.unwrap().path(), secs);
        }
    }
    let at = std::time::SystemTime::now() - Duration::from_secs(secs);
    let mut o = std::fs::OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS (folders too).
        o.access_mode(0x100).custom_flags(0x0200_0000);
    }
    #[cfg(not(windows))]
    o.read(true);
    o.open(p).unwrap().set_modified(at).unwrap();
}

/// A worktree and its own git folder made quiet for three hours.
fn quieten(wt: &Path) {
    let gd = git(wt, &["rev-parse", "--path-format=absolute", "--git-dir"]);
    age(wt, 3 * 3600);
    age(Path::new(gd.trim()), 3 * 3600);
}

#[test]
fn disposable_globs_let_listed_ignored_paths_go_and_keep_the_rest() {
    let fx = Fx::new("gc-disposable");
    std::fs::write(fx.repo.join(".git").join("info").join("exclude"), "backend.cache\n__pycache__/\ncaptures/\n").unwrap();
    let wt = fx.add("dsp");
    std::fs::create_dir_all(wt.join("target")).unwrap();
    std::fs::write(wt.join("target").join("o"), "o").unwrap();
    std::fs::write(wt.join("backend.cache"), "c").unwrap();
    std::fs::create_dir_all(wt.join("tools").join("__pycache__")).unwrap();
    std::fs::write(wt.join("tools").join("__pycache__").join("m.pyc"), "p").unwrap();
    std::fs::write(wt.join("run.log"), "l").unwrap();
    // The default keeps every ignored path but `target/`.
    assert_eq!(GcSettings::default().disposable, vec!["target/".to_string()]);
    assert_eq!(fx.classes(&wt), vec![Work::Ignored]);
    let globs: Vec<String> = ["target/", "backend.cache", "**/__pycache__/", "*.log"].iter().map(|s| s.to_string()).collect();
    let (found, _) = gc::wip(&Probe { disposable: &globs, ..fx.probe(&wt) });
    assert!(found.is_empty(), "{found:?}");
    // An ignored path the list leaves out is still work.
    let (found, _) = gc::wip(&Probe { disposable: &globs[..3], ..fx.probe(&wt) });
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].0 == Work::Ignored && found[0].1.contains("run.log"), "{found:?}");
    // A listed glob never covers an untracked file.
    std::fs::write(wt.join("notes.md"), "n").unwrap();
    let listed: Vec<String> = globs.iter().cloned().chain(["*.md".to_string()]).collect();
    let (found, _) = gc::wip(&Probe { disposable: &listed, ..fx.probe(&wt) });
    assert_eq!(found.iter().map(|(w, _)| *w).collect::<Vec<_>>(), vec![Work::Untracked]);
    // The matching rules.
    let t = ["target/".to_string()];
    assert!(gc::disposable(&t, "target/") && gc::disposable(&t, "target/debug/x"));
    assert!(!gc::disposable(&t, "crates/x/target/") && !gc::disposable(&t, "target") && !gc::disposable(&t, "targets/"));
    let py = ["**/__pycache__/".to_string()];
    assert!(gc::disposable(&py, "__pycache__/") && gc::disposable(&py, "a/b/__pycache__/c.pyc"));
    assert!(!gc::disposable(&["*.cache".to_string()], "sub/x.cache"));
    // The configuration: a list, never every path.
    let base = fx.root.as_path();
    let c = atlas::config::Config::parse(r#"{"worktrees": {"disposable": ["target/", "**/__pycache__/"]}}"#, base).unwrap();
    assert_eq!(GcSettings::from_spec(c.worktrees.as_ref()).disposable, vec!["target/".to_string(), "**/__pycache__/".to_string()]);
    for bad in [r#""**""#, r#""*/""#, r#""/abs""#, r#""../up""#, r#""""#] {
        let j = format!(r#"{{"worktrees": {{"disposable": [{bad}]}}}}"#);
        assert!(atlas::config::Config::parse(&j, base).is_err(), "{j}");
    }
}

#[test]
fn a_worktree_with_no_commits_of_its_own_is_new_however_far_main_moves() {
    let fx = Fx::new("gc-own");
    let wt = fx.add_new("fresh");
    let rows = [row(1, &wt, "L", "fresh")];
    let s = settings(Dirty::Report);
    for name in ["o1", "o2"] {
        let other = fx.add(name);
        fx.land(&other);
    }
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Keep, "{:?}", e[0]);
    assert!(e[0].why.starts_with("new"), "{}", e[0].why);
    assert_eq!(gc::merged(&wt, T, &rows[0].created_at).unwrap(), None);
    // An unreadable ledger time: not its own.
    assert!(!gc::own_head(&wt, T, "not a time").unwrap());
}

#[test]
fn the_plan_walks_files_last_and_only_for_candidates() {
    let fx = Fx::new("gc-walk");
    let quiet = GcSettings { quiet: Duration::from_secs(2 * 3600), ..settings(Dirty::Report) };
    // Not merged, holding work under `report`: the cheap tests answer, and
    // the fresh files are not walked.
    let nm = fx.add("nm");
    std::fs::write(nm.join("a.txt"), "mine\n").unwrap();
    git(&nm, &["commit", "-q", "-am", "mine"]);
    let wk = fx.add("wk");
    std::fs::write(wk.join("wip.txt"), "x\n").unwrap();
    let rows = [row(1, &nm, "L", "nm"), row(2, &wk, "L", "wk")];
    let e = plan_of(&fx, &rows, &quiet, &[], Some(&[]), &[]);
    assert_eq!((e[0].act, e[0].why.as_str()), (Act::Keep, "not merged"));
    assert_eq!(e[1].act, Act::Work, "{:?}", e[1]);
    assert!(!e[1].why.contains("in use"), "{}", e[1].why);
    // A candidate is walked: fresh files keep it.
    let c = fx.add("c");
    let rows = [row(1, &c, "L", "c")];
    let e = plan_of(&fx, &rows, &quiet, &[], Some(&[]), &[]);
    assert!(e[0].act == Act::Keep && e[0].why.contains("a file in it changed"), "{:?}", e[0]);
    // The hook's notify-only pass walks nothing.
    let notify = GcSettings { plan_walks: false, ..quiet.clone() };
    assert_eq!(plan_of(&fx, &rows, &notify, &[], Some(&[]), &[])[0].act, Act::Remove);
    // `target/` is looked at two levels down, no deeper.
    let deep = c.join("target").join("debug").join("deps").join("x");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("f"), "f").unwrap();
    quieten(&c);
    std::fs::write(deep.join("f"), "newer").unwrap();
    std::fs::write(deep.join("g"), "new").unwrap();
    let e = plan_of(&fx, &rows, &quiet, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Remove, "{:?}", e[0]);
    std::fs::write(c.join("target").join("debug").join("deps").join("h"), "new").unwrap();
    let e = plan_of(&fx, &rows, &quiet, &[], Some(&[]), &[]);
    assert!(e[0].act == Act::Keep && e[0].why.contains("a file in it changed"), "{:?}", e[0]);
}

#[test]
fn a_linked_build_folder_changed_within_the_quiet_period_keeps_the_tree() {
    let fx = Fx::new("gc-linked");
    let wt = fx.add("lt");
    let tdir = fx.root.join("targets").join("lt");
    std::fs::create_dir_all(tdir.join("debug")).unwrap();
    quieten(&wt);
    age(&tdir, 3 * 3600);
    let quiet = GcSettings {
        quiet: Duration::from_secs(2 * 3600),
        target_dirs: vec![fx.root.join("targets").join("{name}").to_string_lossy().into_owned()],
        ..settings(Dirty::Report)
    };
    let rows = [row(1, &wt, "L", "lt")];
    assert_eq!(plan_of(&fx, &rows, &quiet, &[], Some(&[]), &[])[0].act, Act::Remove);
    std::fs::write(tdir.join("debug").join("new.rlib"), "x").unwrap();
    let e = plan_of(&fx, &rows, &quiet, &[], Some(&[]), &[]);
    assert!(e[0].act == Act::Keep && e[0].why.contains("its build folder"), "{:?}", e[0]);
}

#[test]
fn two_runs_never_both_hold_the_lock() {
    let fx = Fx::new("gc-lock2");
    let lp = fx.root.join("worktree-gc.lock");
    let start = std::sync::Arc::new(std::sync::Barrier::new(8));
    let held = std::sync::Arc::new(std::sync::Barrier::new(8));
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let (lp, start, held) = (lp.clone(), start.clone(), held.clone());
            std::thread::spawn(move || {
                start.wait();
                let l = gc::RunLock::take(&lp, NOW);
                held.wait();
                l.is_ok()
            })
        })
        .collect();
    let won = hs.into_iter().map(|h| h.join().unwrap()).filter(|w| *w).count();
    assert_eq!(won, 1);
}

#[test]
fn the_apply_loop_reads_the_waiting_queue_again() {
    let fx = Fx::new("gc-requeue");
    let wt = fx.add("rq");
    let rows = [row(1, &wt, "LANE-Q", "rq")];
    let s = settings(Dirty::Report);
    let repos = fx.repos();
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Remove);
    let cx = Context { rows: &rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: &s };
    let item = Item {
        key: "queue:late".into(),
        kind: Kind::Builder,
        class: class(Some("LANE-Q"), &[]),
        waited_minutes: 0,
        order: None,
        takes_slot: true,
        unmet: Vec::new(),
    };
    assert!(gc::apply_with(&cx, &mut e, &|| Some(Vec::new()), &|| Some(vec![item.clone()])).is_empty());
    assert!(e[0].done.as_deref().unwrap_or("").contains("waiting queue item queue:late"), "{:?}", e[0].done);
    assert!(gc::apply_with(&cx, &mut e, &|| Some(Vec::new()), &|| None).is_empty());
    assert!(e[0].done.as_deref().unwrap_or("").contains("waiting queue could not be read"), "{:?}", e[0].done);
    assert!(wt.is_dir());
}

#[test]
fn commit_delete_refuses_a_secret_added_to_a_tracked_file() {
    let fx = Fx::new("gc-tracked");
    let wt = fx.add("tr");
    let s = settings(Dirty::CommitDelete);
    std::fs::write(wt.join("a.txt"), "a\nexport API_TOKEN=notarealvalue123\n").unwrap();
    let err = gc::commit_delete(&fx.repo, &wt, "tr", &[], &s).unwrap_err();
    assert!(err.contains("tracked file adds a line that looks secret"), "{err}");
    assert!(git(&fx.remote, &["branch", "--list", "tr"]).trim().is_empty());
    let small = GcSettings { commit_limit: 8, ..s };
    std::fs::write(wt.join("a.txt"), "a\nplain words that are long\n").unwrap();
    assert!(gc::commit_delete(&fx.repo, &wt, "tr", &[], &small).unwrap_err().contains("changes to tracked files"));
}

#[test]
fn a_build_folder_inside_any_git_checkout_is_refused() {
    let fx = Fx::new("gc-anyrepo");
    let wt = fx.add("ar");
    let other = fx.root.join("unconfigured");
    std::fs::create_dir_all(other.join("plain")).unwrap();
    git(&other, &["init", "-q"]);
    let rows = [row(1, &wt, "L", "ar")];
    let s = settings(Dirty::Report);
    let repos = fx.repos();
    let cx = Context { rows: &rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: &s };
    let r = gc::build_refusal(&cx, &other.join("plain")).unwrap_or_default();
    assert!(r.contains("lies inside the git checkout"), "{r}");
    let free = fx.root.join("free");
    std::fs::create_dir_all(&free).unwrap();
    assert_eq!(gc::build_refusal(&cx, &free), None);
}

/// Writes cargo's cache tag (first line `first`) into `dir`.
fn cache_tag(dir: &Path, first: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("CACHEDIR.TAG"), format!("{first}\n# This file is a cache directory tag created by cargo.\n")).unwrap();
}

#[test]
fn a_cargo_build_folder_untracked_in_a_foreign_repo_may_go_and_nothing_else_there() {
    let fx = Fx::new("gc-cachetag");
    let wt = fx.add("ct");
    let ws = fx.root.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    git(&ws, &["init", "-q"]);
    let rows = [row(1, &wt, "L", "ct")];
    let s = settings(Dirty::Report);
    let repos = fx.repos();
    let cx = Context { rows: &rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: &s };
    let refused = |p: &Path| gc::build_refusal(&cx, p).is_some_and(|r| r.contains("lies inside the git checkout"));
    // Tagged and untracked: cargo's own build folder.
    let ok = ws.join("targets").join("ok");
    cache_tag(&ok, gc::CACHEDIR_TAG_SIGNATURE);
    assert_eq!(gc::build_refusal(&cx, &ok), None);
    // No tag, a wrong signature, a signature with more on its line, or a
    // tag that is not a file: refused.
    let none = ws.join("targets").join("none");
    std::fs::create_dir_all(&none).unwrap();
    assert!(refused(&none));
    let wrong = ws.join("targets").join("wrong");
    cache_tag(&wrong, "Signature: 0000000000000000000000000000000");
    assert!(refused(&wrong));
    let longer = ws.join("targets").join("longer");
    cache_tag(&longer, &format!("{}0", gc::CACHEDIR_TAG_SIGNATURE));
    assert!(refused(&longer));
    let dir_tag = ws.join("targets").join("dirtag");
    std::fs::create_dir_all(dir_tag.join("CACHEDIR.TAG")).unwrap();
    assert!(refused(&dir_tag));
    // Tagged, but the repository tracks a file there: refused.
    let tracked = ws.join("tracked");
    cache_tag(&tracked, gc::CACHEDIR_TAG_SIGNATURE);
    std::fs::write(tracked.join("notes.md"), "mine\n").unwrap();
    git(&ws, &["add", "tracked/notes.md"]);
    assert!(refused(&tracked));
    // The other refusals stay: a tagged folder that holds .git, or is a
    // ledger worktree.
    cache_tag(&wt, gc::CACHEDIR_TAG_SIGNATURE);
    assert!(gc::build_refusal(&cx, &wt).is_some());
    let inner = ws.join("targets").join("inner");
    cache_tag(&inner, gc::CACHEDIR_TAG_SIGNATURE);
    git(&inner, &["init", "-q"]);
    assert!(gc::build_refusal(&cx, &inner).unwrap_or_default().contains("holds .git"));
}

#[test]
fn a_cargo_build_folder_in_a_foreign_repo_is_planned_and_checked_again_before_it_goes() {
    let fx = Fx::new("gc-cachetag-apply");
    let ws = fx.root.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    git(&ws, &["init", "-q"]);
    let s = GcSettings { target_dirs: vec![ws.join("targets").join("{name}").to_string_lossy().into_owned()], ..settings(Dirty::Report) };
    // Tagged at plan and at apply: deleted with its tree.
    let a = fx.add("ca");
    let ta = ws.join("targets").join("ca");
    cache_tag(&ta, gc::CACHEDIR_TAG_SIGNATURE);
    let rows = [row(1, &a, "L", "ca")];
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!((e[0].act, e[0].target.as_ref().map(|t| t.0.clone())), (Act::Remove, Some(ta.clone())), "{:?}", e[0]);
    assert_eq!(apply_all(&fx, &rows, &s, &mut e).len(), 1);
    assert!(!a.exists() && !ta.exists(), "{:?}", e[0].done);
    // Untagged: refused at plan.
    let b = fx.add("cb");
    let tb = ws.join("targets").join("cb");
    std::fs::create_dir_all(&tb).unwrap();
    let rows = [row(2, &b, "L", "cb")];
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert!(e[0].target.is_none() && e[0].why.contains("target refused"), "{:?}", e[0]);
    // Tagged at plan, the tag gone by apply: refused again, left in place.
    let c = fx.add("cc");
    let tc = ws.join("targets").join("cc");
    cache_tag(&tc, gc::CACHEDIR_TAG_SIGNATURE);
    let rows = [row(3, &c, "L", "cc")];
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert!(e[0].target.is_some(), "{:?}", e[0]);
    std::fs::remove_file(tc.join("CACHEDIR.TAG")).unwrap();
    apply_all(&fx, &rows, &s, &mut e);
    assert!(tc.is_dir(), "{:?}", e[0].done);
    assert!(e[0].done.as_deref().unwrap_or("").contains("target left: refused"), "{:?}", e[0].done);
}

#[test]
fn a_tree_made_at_a_pr_head_stays_new_when_that_pr_ends() {
    let fx = Fx::new("gc-prhead");
    // The PR's commit, made before the follow-up tree (its ledger row is
    // from 2022).
    let pr = fx.add_new("pr");
    std::fs::write(pr.join("pr.txt"), "pr\n").unwrap();
    git(&pr, &["add", "-A"]);
    let o = Command::new("git")
        .arg("-C")
        .arg(&pr)
        .args(["commit", "-q", "-m", "pr work"])
        .env("GIT_AUTHOR_DATE", "2021-06-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2021-06-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    git(&pr, &["push", "-q", "origin", "pr"]);
    git(&fx.repo, &["fetch", "-q", "origin"]);
    let head = git(&pr, &["rev-parse", "HEAD"]).trim().to_string();
    let follow = fx.root.join("wts").join("follow");
    git(&fx.repo, &["worktree", "add", "-q", "--no-track", "-b", "follow", &follow.to_string_lossy(), "origin/pr"]);
    let rows = [row(1, &follow, "L", "follow")];
    let s = settings(Dirty::Report);
    let ends = [PrEnd { pr: 9, verdict: "merged".into(), head }];
    let e = plan_of(&fx, &rows, &s, &ends, Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Keep, "{:?}", e[0]);
    assert!(e[0].why.starts_with("new"), "{}", e[0].why);
    assert!(follow.is_dir());
}

#[test]
fn a_leftover_auto_merge_is_not_an_operation_in_progress() {
    let fx = Fx::new("gc-automerge");
    let wt = fx.add("am");
    let own = git(&wt, &["rev-parse", "--path-format=absolute", "--git-dir"]);
    let own = PathBuf::from(own.trim());
    // A finished rebase can leave AUTO_MERGE behind; git status ignores it.
    let tip = git(&wt, &["rev-parse", "HEAD^{tree}"]);
    std::fs::write(own.join("AUTO_MERGE"), tip).unwrap();
    assert_eq!(fx.classes(&wt), Vec::<Work>::new());
    assert_eq!(gc::in_progress(&wt, T).unwrap(), None);
    // A real rebase still is.
    std::fs::create_dir_all(own.join("rebase-merge")).unwrap();
    fx.kept_for(&wt, &[Work::InProgress]);
}

#[test]
fn disposable_globs_made_only_of_wildcards_are_refused() {
    let fx = Fx::new("gc-wildglob");
    let base = fx.root.as_path();
    for bad in ["?*", "*?", "?*/", "**/?*", "?", "*/?/**"] {
        let j = format!(r#"{{"worktrees": {{"disposable": ["{bad}"]}}}}"#);
        assert!(atlas::config::Config::parse(&j, base).is_err(), "{j}");
    }
    for good in ["t?rget/", "*.cache", "**/__pycache__/", "?ut/"] {
        let j = format!(r#"{{"worktrees": {{"disposable": ["{good}"]}}}}"#);
        assert!(atlas::config::Config::parse(&j, base).is_ok(), "{j}");
    }
}

/// A tagged build folder at `dir` with plain build output in it.
fn build_folder(dir: &Path) {
    cache_tag(dir, gc::CACHEDIR_TAG_SIGNATURE);
    std::fs::create_dir_all(dir.join("debug").join("deps")).unwrap();
    std::fs::write(dir.join("debug").join("deps").join("x.o"), "o").unwrap();
}

#[test]
fn a_build_folder_holding_a_checkout_at_any_depth_is_refused() {
    let fx = Fx::new("gc-nestedgit");
    let wt = fx.add("ng");
    let ws = fx.root.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    git(&ws, &["init", "-q"]);
    let rows = [row(1, &wt, "L", "ng")];
    let s = settings(Dirty::Report);
    let repos = fx.repos();
    let cx = Context { rows: &rows, repos: &repos, holders: Some(&[]), waiting: &[], pr_ends: &[], settings: &s };
    // Inside a foreign repo (tagged, untracked) and outside any repo.
    for base in [ws.join("targets"), fx.root.join("free")] {
        // Plain build output: it may go.
        let ok = base.join("ok");
        build_folder(&ok);
        assert_eq!(gc::build_refusal(&cx, &ok), None, "{}", ok.display());
        // A clone or `git init` two levels down, with uncommitted work.
        let dir = base.join("dir");
        build_folder(&dir);
        let scratch = dir.join("sub").join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        git(&scratch, &["init", "-q"]);
        std::fs::write(scratch.join("work.rs"), "fn wip() {}\n").unwrap();
        let r = gc::build_refusal(&cx, &dir).unwrap_or_default();
        assert!(r.contains("holds .git at"), "{r}");
        // A `.git` file (a worktree's link) two levels down.
        let file = base.join("file");
        build_folder(&file);
        std::fs::create_dir_all(file.join("sub").join("wt")).unwrap();
        std::fs::write(file.join("sub").join("wt").join(".git"), "gitdir: /nowhere/.git/worktrees/wt\n").unwrap();
        let r = gc::build_refusal(&cx, &file).unwrap_or_default();
        assert!(r.contains("holds .git at"), "{r}");
    }
}

#[test]
fn a_checkout_made_in_a_build_folder_after_the_plan_stops_its_removal() {
    let fx = Fx::new("gc-nestedgit-apply");
    let ws = fx.root.join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    git(&ws, &["init", "-q"]);
    let s = GcSettings { target_dirs: vec![ws.join("targets").join("{name}").to_string_lossy().into_owned()], ..settings(Dirty::Report) };
    let a = fx.add("na");
    let ta = ws.join("targets").join("na");
    build_folder(&ta);
    let rows = [row(1, &a, "L", "na")];
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert!(e[0].target.is_some(), "{:?}", e[0]);
    let scratch = ta.join("sub").join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    git(&scratch, &["init", "-q"]);
    std::fs::write(scratch.join("work.rs"), "fn wip() {}\n").unwrap();
    apply_all(&fx, &rows, &s, &mut e);
    assert!(scratch.join("work.rs").is_file(), "{:?}", e[0].done);
    assert!(e[0].done.as_deref().unwrap_or("").contains("target left: refused"), "{:?}", e[0].done);
}

#[test]
fn a_disposable_glob_never_makes_an_ignored_secret_disposable() {
    let fx = Fx::new("gc-dispsecret");
    std::fs::write(fx.repo.join(".git").join("info").join("exclude"), ".env\n.env.*\n*.log\n").unwrap();
    let wt = fx.add("dse");
    std::fs::write(wt.join("run.log"), "l").unwrap();
    let globs: Vec<String> = ["*.log", ".env*", "**/.*"].iter().map(|s| s.to_string()).collect();
    // The control: a listed log goes.
    let (found, _) = gc::wip(&Probe { disposable: &globs, ..fx.probe(&wt) });
    assert!(found.is_empty(), "{found:?}");
    for name in [".env", ".env.local"] {
        std::fs::write(wt.join(name), "KEY=x\n").unwrap();
        let (found, _) = gc::wip(&Probe { disposable: &globs, ..fx.probe(&wt) });
        assert_eq!(found.iter().map(|(w, _)| *w).collect::<Vec<_>>(), vec![Work::Ignored], "{name}: {found:?}");
        assert!(found[0].1.contains(name), "{found:?}");
        std::fs::remove_file(wt.join(name)).unwrap();
    }
    // The matcher itself, at any depth.
    for p in [".env", ".env.production", "config/.env", "a/b/.env.local", ".envrc", "a/.ENVRC"] {
        assert!(!gc::disposable(&globs, p), "{p}");
    }
    assert!(gc::disposable(&globs, "a.log") && gc::disposable(&globs, ".cache"));
    // Other denied names (a key, credentials) as files.
    let keys: Vec<String> = ["id_*", "**/credentials.*", "k/"].iter().map(|s| s.to_string()).collect();
    for p in ["id_rsa", "a/credentials.json", "k/id_ed25519"] {
        assert!(!gc::disposable(&keys, p), "{p}");
    }
    assert!(gc::disposable(&keys, "k/") && gc::disposable(&keys, "k/out.bin"));
}

/// Ruling ATLAS-RELEASE 0219 ("100% safe from deleting wip"): git lists an
/// ignored folder whole, so a folder a `disposable` glob matches is walked
/// for a secret or a `.git` before it counts as disposable, at the plan and
/// again at the removal.
#[test]
fn a_secret_or_a_checkout_inside_a_disposable_folder_keeps_the_tree() {
    let fx = Fx::new("gc-dispwalk");
    std::fs::write(fx.repo.join(".git").join("info").join("exclude"), "cache/\n").unwrap();
    let wt = fx.add("dw");
    let deep = wt.join("cache").join("deep");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("blob.bin"), "b").unwrap();
    let globs: Vec<String> = vec!["cache/".into()];
    // By name the folder is disposable: wip walks nothing.
    assert!(gc::wip(&Probe { disposable: &globs, ..fx.probe(&wt) }).0.is_empty());
    let s = GcSettings { disposable: globs.clone(), ..settings(Dirty::Report) };
    let rows = [row(1, &wt, "L", "dw")];
    let planned = || plan_of(&fx, &rows, &s, &[], Some(&[]), &[]).remove(0);
    // The control: the same folder with nothing secret in it goes.
    assert_eq!(planned().act, Act::Remove);
    for (name, dir) in [(".env", false), ("id_rsa", false), (".git", true), (".hg", true), (".envrc", false)] {
        let p = deep.join(name);
        if dir {
            std::fs::create_dir_all(&p).unwrap();
        } else {
            std::fs::write(&p, "KEY=x\n").unwrap();
        }
        let e = planned();
        let found: Vec<Work> = e.found.iter().map(|(w, _)| *w).collect();
        assert!(e.act == Act::Work && found == [Work::Ignored] && e.found[0].1.contains("cache"), "{name}: {e:?}");
        let r = gc::remove_tree(&fx.repo, &wt, &[], &globs, T);
        assert!(r.is_err() && p.exists(), "{name}: removed through a disposable folder");
        if dir {
            std::fs::remove_dir_all(&p).unwrap();
        } else {
            std::fs::remove_file(&p).unwrap();
        }
    }
    assert_eq!(planned().act, Act::Remove);
}

/// A `git init` with uncommitted work at `dir/sub/scratch`: its file.
fn scratch_checkout(dir: &Path) -> PathBuf {
    let scratch = dir.join("sub").join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    git(&scratch, &["init", "-q"]);
    std::fs::write(scratch.join("work.rs"), "fn wip() {}\n").unwrap();
    scratch.join("work.rs")
}

/// Review r5 S1: `remove_target` deletes the in-tree `target/` only through
/// the guard, so a checkout made in it stays.
#[test]
fn remove_target_never_deletes_a_checkout_inside_the_tree_target() {
    let fx = Fx::new("gc-s1");
    let wt = fx.add("s1");
    std::fs::write(wt.join("mine.txt"), "x\n").unwrap();
    let work = scratch_checkout(&wt.join("target"));
    let s = settings(Dirty::RemoveTarget);
    let rows = [row(1, &wt, "L", "s1")];
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Target, "{:?}", e[0]);
    apply_all(&fx, &rows, &s, &mut e);
    assert!(work.is_file(), "{:?}", e[0].done);
    assert!(e[0].done.as_deref().unwrap_or("").contains("refused"), "{:?}", e[0].done);
}

/// Review r5 S2: a `target_dirs` folder inside its own worktree is walked
/// with the tree, at the plan and by the guard before `git worktree remove`.
#[test]
fn a_build_folder_inside_its_own_worktree_holding_a_checkout_keeps_the_tree() {
    let fx = Fx::new("gc-s2");
    std::fs::write(fx.repo.join(".git").join("info").join("exclude"), "out/\n").unwrap();
    let wt = fx.add("s2");
    let out = wt.join("out");
    build_folder(&out);
    let s = GcSettings {
        target_dirs: vec![fx.root.join("wts").join("{name}").join("out").to_string_lossy().into_owned()],
        ..settings(Dirty::Report)
    };
    let rows = [row(1, &wt, "L", "s2")];
    let work = scratch_checkout(&out);
    let e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert!(e[0].act == Act::Work && e[0].found[0].1.contains("holds .git at"), "{:?}", e[0]);
    let r = gc::remove_tree(&fx.repo, &wt, std::slice::from_ref(&out), &fx.globs, T);
    assert!(r.as_ref().is_err_and(|r| r.contains("refused")) && work.is_file(), "{r:?}");
    // The control: plain build output there goes with the tree.
    std::fs::remove_dir_all(out.join("sub")).unwrap();
    let mut e = plan_of(&fx, &rows, &s, &[], Some(&[]), &[]);
    assert_eq!(e[0].act, Act::Remove, "{:?}", e[0]);
    apply_all(&fx, &rows, &s, &mut e);
    assert!(!wt.exists(), "{:?}", e[0].done);
}

/// What gc's source scan looks for: the filesystem deletes, git's worktree
/// removal, and (#351 F8) a delete by another command's argument (`git
/// clean`, `git worktree prune`, a shell's `rm`, `rmdir`, `rd` or `del`),
/// each as a quoted argument so that no word merely holding the letters
/// matches. A string `"clean"` in gc's source (say a status name) trips the
/// scan: name it otherwise, or allow-list it here.
const DELETE_NEEDLES: [&str; 10] =
    ["remove_dir_all", "remove_file", "remove_dir(", "\"worktree\", \"remove\"", "\"clean\"", "\"prune\"", "\"rmdir\"", "\"rm\"", "\"rd\"", "\"del\""];

/// The needles in `src`, inside `pub fn guarded_delete(` and outside it
/// (`LINE: text`), comments skipped. A `"worktree",` / `"remove"` pair that
/// rustfmt wrapped onto two lines counts on the second (#355 N5).
fn deletes_in(src: &str) -> (Vec<&'static str>, Vec<String>) {
    const WT_REMOVE: &str = "\"worktree\", \"remove\"";
    let (mut inside, mut outside) = (Vec::new(), Vec::new());
    let mut in_guard = false;
    let mut wrapped = false;
    for (i, l) in src.lines().enumerate() {
        in_guard |= l.starts_with("pub fn guarded_delete(");
        let t = l.trim();
        if !t.starts_with("//") && !t.is_empty() {
            let pair = wrapped && t.starts_with("\"remove\"");
            wrapped = t.ends_with("\"worktree\",");
            let split = pair.then_some(&WT_REMOVE);
            for n in DELETE_NEEDLES.iter().filter(|n| l.contains(**n)).chain(split) {
                if in_guard {
                    inside.push(*n)
                } else {
                    outside.push(format!("{}: {l}", i + 1))
                }
            }
        }
        in_guard &= l != "}";
    }
    (inside, outside)
}

/// Every filesystem delete in gc is inside `guarded_delete`; the Docker
/// module deletes only through docker.
#[test]
fn every_gc_delete_goes_through_the_one_guard() {
    let (inside, outside) = deletes_in(include_str!("../src/worktree_gc.rs"));
    assert!(outside.is_empty(), "deletes outside guarded_delete: {outside:#?}");
    for n in ["remove_dir_all", "remove_file", "\"worktree\", \"remove\""] {
        assert!(inside.contains(&n), "guarded_delete no longer holds {n}");
    }
    // The scan sees each new needle as a command's argument, and no prose.
    for l in [
        "    git(wt, &[\"clean\", \"-fdx\"], t)?;",
        "    git(repo, &[\"worktree\", \"prune\"], t)?;",
        "    Command::new(\"rm\").arg(\"-rf\").arg(p);",
        "    Command::new(\"cmd\").args([\"/C\", \"rmdir\", \"/S\"]);",
        "    Command::new(\"cmd\").args([\"/C\", \"rd\", \"/S\"]);",
        "    Command::new(\"cmd\").args([\"/C\", \"del\", \"/Q\"]);",
    ] {
        assert_eq!(deletes_in(l).1.len(), 1, "not caught: {l}");
    }
    // #355 N5: rustfmt's wrap of the pair, and inside the guard too.
    let wrapped = "    git(\n        repo,\n        &[\n            \"worktree\",\n            \"remove\",\n            \"--force\",\n        ],\n    )?;";
    assert_eq!(deletes_in(wrapped).1.len(), 1, "not caught: {wrapped}");
    let guard = format!("pub fn guarded_delete() {{\n{wrapped}\n}}");
    assert_eq!(deletes_in(&guard).0, ["\"worktree\", \"remove\""]);
    assert!(deletes_in("    let s = format!(\"a clean, pruned perm form: {rm}\");").1.is_empty());
    let docker = include_str!("../src/worktree_gc_docker.rs");
    assert!(!["remove_dir", "remove_file", "fs::remove", "\"prune\"", "\"clean\"", "\"rmdir\""].iter().any(|n| docker.contains(n)));
}

/// Review r5 S3: the notify-only plan and a row gc keeps walk nothing.
#[test]
fn a_notify_only_plan_and_a_kept_row_walk_nothing() {
    let fx = Fx::new("gc-s3");
    let c = fx.add("c3");
    std::fs::create_dir_all(c.join("target").join("debug")).unwrap();
    let wk = fx.add("wk3");
    std::fs::write(wk.join("wip.txt"), "x\n").unwrap();
    for n in ["c3", "wk3"] {
        build_folder(&fx.root.join("targets").join(n));
    }
    let s =
        GcSettings { target_dirs: vec![fx.root.join("targets").join("{name}").to_string_lossy().into_owned()], ..settings(Dirty::Report) };
    let notify = GcSettings { plan_walks: false, ..s.clone() };
    let walked = |rows: &[Row], s: &GcSettings| {
        let before = gc::walks();
        let e = plan_of(&fx, rows, s, &[], Some(&[]), &[]);
        (gc::walks() - before, e.into_iter().map(|e| e.act).collect::<Vec<_>>())
    };
    let both = [row(1, &c, "L", "c3"), row(2, &wk, "L", "wk3")];
    assert_eq!(walked(&both, &notify), (0, vec![Act::Remove, Act::Work]));
    assert_eq!(walked(&both[1..], &s), (0, vec![Act::Work]));
    // The control: a row planned for removal is walked (tree, linked folder).
    assert_eq!(walked(&both[..1], &s), (2, vec![Act::Remove]));
}

/// The guard's own refusals: a path of the wrong kind, a secret, a link.
#[test]
fn the_guard_refuses_what_is_not_its_kind_and_a_gone_path_is_done() {
    let t = Temp::new("gc-guard");
    let dir = t.path().join("d");
    std::fs::create_dir_all(&dir).unwrap();
    let file = t.path().join("summary.tmp1");
    std::fs::write(&file, "1 2\n").unwrap();
    let env = t.path().join(".env");
    std::fs::write(&env, "KEY=x\n").unwrap();
    for d in [gc::Delete::OwnFile(&dir), gc::Delete::OwnFile(&env), gc::Delete::Folder(&file)] {
        assert!(gc::guarded_delete(&d).is_err_and(|e| e.contains("refused")), "{d:?}");
    }
    assert!(dir.is_dir() && env.is_file() && file.is_file());
    gc::guarded_delete(&gc::Delete::OwnFile(&file)).unwrap();
    gc::guarded_delete(&gc::Delete::Folder(&dir)).unwrap();
    assert!(!file.exists() && !dir.exists());
    gc::guarded_delete(&gc::Delete::Folder(&dir)).unwrap();
}

/// A link or junction to `target` at `link`.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let o = Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// Review r5 N3 and N5: links are never entered (nor deleted through), a
/// name that is not UTF-8 is a hit, and other checkouts count.
#[test]
fn the_walk_enters_no_link_and_knows_other_checkouts_and_odd_names() {
    let t = Temp::new("gc-walkprops");
    let outside = t.path().join("outside");
    std::fs::create_dir_all(outside.join(".git")).unwrap();
    std::fs::write(outside.join("keep.txt"), "k").unwrap();
    let b = t.path().join("b");
    build_folder(&b);
    link_dir(&outside, &b.join("link"));
    assert_eq!(gc::delete_refusal(&gc::Delete::Folder(&b)), None);
    // A link as the folder itself is refused, not followed.
    let alias = t.path().join("alias");
    link_dir(&b, &alias);
    assert!(gc::delete_refusal(&gc::Delete::Folder(&alias)).is_some());
    gc::guarded_delete(&gc::Delete::Folder(&b)).unwrap();
    assert!(!b.exists() && outside.join("keep.txt").is_file() && outside.join(".git").is_dir());
    let hit = |setup: &dyn Fn(&Path), want: &str| {
        let d = t.path().join("hit");
        build_folder(&d);
        setup(&d.join("debug"));
        let r = gc::delete_refusal(&gc::Delete::Folder(&d)).unwrap_or_default();
        assert!(r.contains(want), "{want}: {r}");
        std::fs::remove_dir_all(&d).unwrap();
    };
    for n in [".hg", ".jj", ".SVN"] {
        hit(&|d| std::fs::create_dir_all(d.join(n)).unwrap(), n);
    }
    hit(
        &|d| {
            git(d, &["init", "-q", "--bare", "bare"]);
        },
        "a bare git repository",
    );
    hit(&|d| std::fs::write(d.join(".git-credentials"), "https://x").unwrap(), "a secret");
    #[cfg(windows)]
    let odd = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xD800, u16::from(b'x')])
    };
    #[cfg(unix)]
    let odd = {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(b"x\xff").to_os_string()
    };
    // A file system that refuses the name (macOS APFS: EILSEQ) cannot hold
    // such a file, so there is nothing for the walk to meet there.
    let probe = t.path().join("probe");
    std::fs::create_dir_all(&probe).unwrap();
    if std::fs::write(probe.join(&odd), "o").is_ok() {
        hit(&|d| std::fs::write(d.join(&odd), "o").unwrap(), "a secret");
    }
}

/// #351 F2: a folder git tracks as a gitlink (an embedded repository or a
/// submodule) is walked like any other, so the guard itself refuses a tree
/// with one checked out, on the gitlink's own `.git`, clean or not (#355
/// N1), before git is asked.
#[test]
fn the_guard_walks_into_a_tracked_gitlink_and_refuses_its_nested_git() {
    let fx = Fx::new("gc-gitlink");
    let wt = fx.add("gl2");
    let sub = wt.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"]);
    std::fs::write(sub.join("s.txt"), "s\n").unwrap();
    let hooks = format!("core.hooksPath={}", fx.root.join("no-hooks").display());
    let me = ["-c", "user.name=atlas-test", "-c", "user.email=atlas-test@example.invalid", "-c", "commit.gpgsign=false", "-c", &hooks];
    git(&sub, &[&me[..], &["add", "-A"]].concat());
    git(&sub, &[&me[..], &["commit", "-q", "-m", "sub"]].concat());
    // The parent tracks `sub` as one gitlink path (mode 160000).
    git(&wt, &["add", "sub"]);
    git(&wt, &["commit", "-q", "-m", "embed sub"]);
    assert!(git(&wt, &["ls-files", "-s", "sub"]).starts_with("160000"), "sub is not a gitlink");
    let d = gc::Delete::Worktree { repo: &fx.repo, wt: &wt, timeout: T };
    // A clean gitlink is refused too: the nested `.git` is the hit.
    assert!(git(&sub, &["status", "--porcelain"]).trim().is_empty(), "sub is not clean");
    let r = gc::delete_refusal(&d).expect("the walk refuses a tree with a clean gitlink checked out");
    assert!(r.contains(&format!("sub{}.git", std::path::MAIN_SEPARATOR)), "{r}");
    std::fs::write(sub.join("wip.txt"), "uncommitted\n").unwrap();
    let r = gc::delete_refusal(&d).expect("the walk refuses a tree with a checkout in a gitlink");
    assert!(r.contains(".git"), "{r}");
    let e = gc::guarded_delete(&d).expect_err("a tree with work in a gitlink was removed");
    assert!(e.starts_with("refused:"), "refused by git, not by the guard: {e}");
    assert!(sub.join("wip.txt").is_file() && wt.is_dir());
}

/// #351 F6 (review r5 N3, #355 S1): an entry the walk cannot read is a
/// refusal, never "holds nothing". Every platform: the read of one folder
/// fails through the walk's test seam, and the file below it stays.
#[test]
fn an_unreadable_folder_in_the_walk_is_a_refusal() {
    let t = Temp::new("gc-unreadable-seam");
    let b = t.path().join("b");
    build_folder(&b);
    let locked = b.join("debug").join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("x.o"), "o").unwrap();
    gc::inject_walk_error(Some(&locked));
    let r = gc::delete_refusal(&gc::Delete::Folder(&b));
    let e = gc::guarded_delete(&gc::Delete::Folder(&b));
    gc::inject_walk_error(None);
    assert!(r.as_deref().is_some_and(|r| r.contains("could not be walked") && r.contains("locked")), "{r:?}");
    assert!(e.as_ref().is_err_and(|e| e.starts_with("refused:")), "{e:?}");
    assert!(locked.join("x.o").is_file() && b.is_dir());
    // The seam cleared, the same folder is plain output.
    assert_eq!(gc::delete_refusal(&gc::Delete::Folder(&b)), None);
}

/// The same with a real mode-000 folder (Unix). Root reads one anyway (the
/// linux-test container runs as root): then it is skipped, loudly, and
/// `ATLAS_REQUIRE_UNREADABLE=1` makes the skip a failure.
#[cfg(unix)]
#[test]
fn an_unreadable_folder_by_mode_is_a_refusal() {
    use std::os::unix::fs::PermissionsExt;
    let t = Temp::new("gc-unreadable");
    let b = t.path().join("b");
    build_folder(&b);
    let locked = b.join("debug").join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("x.o"), "o").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let open = |p: &Path| std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    if std::fs::read_dir(&locked).is_ok() {
        open(&locked);
        assert!(std::env::var("ATLAS_REQUIRE_UNREADABLE").as_deref() != Ok("1"), "ATLAS_REQUIRE_UNREADABLE=1 but this user reads a mode-000 folder (root)");
        eprintln!("SKIPPED an_unreadable_folder_by_mode_is_a_refusal: this user reads a mode-000 folder (root)");
        return;
    }
    let r = gc::delete_refusal(&gc::Delete::Folder(&b));
    let e = gc::guarded_delete(&gc::Delete::Folder(&b));
    open(&locked);
    assert!(r.as_deref().is_some_and(|r| r.contains("could not be walked")), "{r:?}");
    assert!(e.is_err_and(|e| e.starts_with("refused:")));
    assert!(locked.join("x.o").is_file());
}
