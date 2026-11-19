//! Tests of [`super`]: the resolution order, the gh seam's failures and the
//! parked mark, on scripted gh answers and fake `.git` files (no process,
//! no network).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn fresh_dir(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("atlas-card-agents-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn store(d: &Path) -> MemoryStore {
    MemoryStore::open(&d.join("memory.sqlite")).unwrap()
}

/// Files an entry in `lane` at `time`.
fn put(s: &mut MemoryStore, kind: &str, sub: &str, lane: &str, time: &str, text: &str, meta: &str) {
    let d = crate::memory::Draft {
        title: text.lines().next().unwrap_or("").to_string(),
        meta: meta.into(),
        body: text.into(),
        reason: "t".into(),
        author: "t".into(),
        client: "t".into(),
        time: Some(time.into()),
        ..Default::default()
    };
    let mut w = s.writer().unwrap();
    let key = w.next_key(&format!("{kind}:{lane}:"), 4).unwrap();
    w.create(&key, kind, sub, lane, &d, None).unwrap();
    w.commit().unwrap();
}

/// A linked worktree's files: `.git` names a gitdir whose HEAD is on
/// `branch`, its sha in the common dir's packed refs.
fn worktree(root: &Path, name: &str, branch: &str, sha: &str) -> PathBuf {
    let common = root.join("main.git");
    let gd = common.join("worktrees").join(name);
    std::fs::create_dir_all(&gd).unwrap();
    std::fs::write(gd.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    std::fs::write(gd.join("commondir"), "../..\n").unwrap();
    let packed = common.join("packed-refs");
    let old = std::fs::read_to_string(&packed).unwrap_or_default();
    std::fs::write(&packed, format!("{old}{sha} refs/heads/{branch}\n")).unwrap();
    let wt = root.join(name);
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gd.display())).unwrap();
    wt
}

const LAUNCH: &str = "2026-11-18T10:00:00.000Z";
const SHA: &str = "abcdef0123456789abcdef0123456789abcdef01";

fn agent(id: &str, lane: &str, wt: &str, path: &Path, running: bool) -> Agent {
    Agent {
        id: id.into(),
        lane: lane.into(),
        launched: LAUNCH.into(),
        worktree: wt.into(),
        path: path.to_string_lossy().into_owned(),
        running,
    }
}

/// The default deadline.
fn dl() -> Instant {
    Instant::now() + GH_BUDGET
}

/// A gh that must not be asked.
fn no_gh() -> Arc<GhList> {
    Arc::new(|_: &Path, b: &str, _| panic!("gh asked for {b}"))
}

/// A gh that answers `pr` for `branch` and nothing for any other, counting
/// its calls.
fn gh_for(branch: &'static str, pr: u64, calls: Arc<AtomicUsize>) -> Arc<GhList> {
    Arc::new(move |_: &Path, b: &str, _| {
        calls.fetch_add(1, Ordering::Relaxed);
        Ok((b == branch).then(|| Pr { number: pr, head: SHA.into(), draft: Some(false) }))
    })
}

#[test]
fn a_pr_journal_entry_after_the_launch_comes_first() {
    let d = fresh_dir("journal");
    let mut s = store(&d);
    put(&mut s, "journal", "note", "L", "2026-11-18T09:00:00.000Z", "pr:300 head:1111111", "");
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "pr:330 head:2222222aaa", "");
    let (items, note) = super::items(&s, &[agent("a1", "L", "", Path::new(""), true)], &no_gh(), dl()).unwrap();
    assert_eq!((items, note), (vec!["L PR #330 @2222222".to_string()], String::new()));
}

#[test]
fn a_pr_in_meta_lines_is_read_too() {
    let d = fresh_dir("meta");
    let mut s = store(&d);
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "opened the PR", "pr: 331\nhead: 3333333\nstate: draft");
    let (items, _) = super::items(&s, &[agent("a1", "L", "", Path::new(""), true)], &no_gh(), dl()).unwrap();
    assert_eq!(items, ["L PR #331 @3333333 (draft)"]);
}

#[test]
fn without_a_journal_entry_the_worktree_branch_asks_gh_once() {
    let d = fresh_dir("branch");
    let s = store(&d);
    let wt = worktree(&d, "card-prs-1", "card-live-prs", SHA);
    let calls = Arc::new(AtomicUsize::new(0));
    let (items, note) =
        super::items(&s, &[agent("a1", "L", "card-prs-1", &wt, true)], &gh_for("card-live-prs", 332, calls.clone()), dl()).unwrap();
    assert_eq!((items, note), (vec!["L PR #332 @abcdef0 (ready)".to_string()], String::new()));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn a_pr_event_for_the_worktree_head_spares_gh() {
    let d = fresh_dir("event");
    let mut s = store(&d);
    let wt = worktree(&d, "w", "b", SHA);
    put(
        &mut s,
        "journal",
        "note",
        "pr-events",
        "2026-11-18T11:00:00.000Z",
        "PR #333 reviewed",
        &format!("event: pull_request_review\nrepo: o/a\npr: 333\nhead: {SHA}\nverdict: approved"),
    );
    let (items, _) = super::items(&s, &[agent("a1", "L", "w", &wt, true)], &no_gh(), dl()).unwrap();
    assert_eq!(items, ["L PR #333 @abcdef0"]);
    // Merged since: not open, so gh is asked.
    put(
        &mut s,
        "journal",
        "note",
        "pr-events",
        "2026-11-18T12:00:00.000Z",
        "PR #333 merged",
        &format!("event: pull_request\nrepo: o/a\npr: 333\nhead: {SHA}\nverdict: merged"),
    );
    let (items, _) = super::items(&s, &[agent("a1", "L", "w", &wt, true)], &gh_for("x", 1, Arc::new(AtomicUsize::new(0))), dl()).unwrap();
    assert_eq!(items, ["L"]);
}

#[test]
fn a_slow_gh_is_cut_at_the_budget_and_said() {
    let d = fresh_dir("slow");
    let s = store(&d);
    let wt = worktree(&d, "w", "b", SHA);
    let slow: Arc<GhList> = Arc::new(|_: &Path, _: &str, _| {
        std::thread::sleep(Duration::from_secs(4));
        Ok(Some(Pr { number: 9, head: SHA.into(), draft: None }))
    });
    let t = Instant::now();
    let (items, note) = super::items(&s, &[agent("a1", "L", "w", &wt, true)], &slow, dl()).unwrap();
    assert!(t.elapsed() < GH_BUDGET + Duration::from_millis(900), "{:?}", t.elapsed());
    assert_eq!((items, note), (vec!["L".to_string()], "gh over 1.5 s".to_string()));
}

#[test]
fn gh_absent_or_logged_out_leaves_the_line_and_says_why() {
    let d = fresh_dir("absent");
    let s = store(&d);
    let wt = worktree(&d, "w", "b", SHA);
    for (fail, want) in
        [(GhFail::Missing, "gh not found"), (GhFail::NoAuth, "gh not logged in"), (GhFail::Failed("no network".into()), "gh: no network")]
    {
        let gh: Arc<GhList> = Arc::new(move |_: &Path, _: &str, _| Err(fail.clone()));
        let (items, note) = super::items(&s, &[agent("a1", "L", "w", &wt, true)], &gh, dl()).unwrap();
        assert_eq!((items, note), (vec!["L".to_string()], want.to_string()));
    }
}

#[test]
fn a_branch_with_no_pr_is_named_alone_with_no_note() {
    let d = fresh_dir("nopr");
    let s = store(&d);
    let wt = worktree(&d, "w", "b", SHA);
    let (items, note) =
        super::items(&s, &[agent("a1", "L", "w", &wt, true)], &gh_for("other", 1, Arc::new(AtomicUsize::new(0))), dl()).unwrap();
    assert_eq!((items, note), (vec!["L".to_string()], String::new()));
}

#[test]
fn a_worktree_that_no_longer_exists_is_said() {
    let d = fresh_dir("gone");
    let s = store(&d);
    let (items, note) = super::items(&s, &[agent("a1", "L", "w9", &d.join("w9"), true)], &no_gh(), dl()).unwrap();
    assert_eq!((items, note), (vec!["L".to_string()], "worktree w9 gone".to_string()));
}

#[test]
fn two_agents_on_one_lane_get_their_own_prs() {
    let d = fresh_dir("two");
    let mut s = store(&d);
    let w1 = worktree(&d, "w1", "b1", SHA);
    let w2 = worktree(&d, "w2", "b2", "1234567890123456789012345678901234567890");
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "pr:340 head:4444444 worktree:w1", "");
    // The second agent's PR from gh; a `pr:` entry naming neither worktree
    // is no one's on a shared lane.
    put(&mut s, "journal", "note", "L", "2026-11-18T11:30:00.000Z", "pr:999", "");
    let calls = Arc::new(AtomicUsize::new(0));
    let agents = [agent("a1", "L", "w1", &w1, true), agent("a2", "L", "w2", &w2, true)];
    let (items, _) = super::items(&s, &agents, &gh_for("b2", 341, calls.clone()), dl()).unwrap();
    assert_eq!(items, ["L/w1 PR #340 @4444444", "L/w2 PR #341 @abcdef0 (ready)"]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn a_finished_agent_without_a_hand_back_is_parked() {
    let d = fresh_dir("parked");
    let mut s = store(&d);
    // Nothing since the launch: no evidence it is parked, so it leaves.
    put(&mut s, "journal", "note", "P", "2026-11-18T09:00:00.000Z", "waiting: older work", "");
    let (items, _) = super::items(&s, &[agent("a1", "P", "", Path::new(""), false)], &no_gh(), dl()).unwrap();
    assert!(items.is_empty(), "{items:?}");
    // Its newest entry is a wait: parked, with its PR.
    put(&mut s, "journal", "note", "P", "2026-11-18T11:00:00.000Z", "pr:350", "");
    put(&mut s, "journal", "note", "P", "2026-11-18T11:05:00.000Z", "waiting: mutants batch 1", "");
    let (items, _) = super::items(&s, &[agent("a1", "P", "", Path::new(""), false)], &no_gh(), dl()).unwrap();
    assert_eq!(items, ["P parked (own job running) PR #350"]);
    // Work journaled after the wait: no longer waiting, so it leaves.
    put(&mut s, "journal", "measurement", "P", "2026-11-18T12:00:00.000Z", "mutants: 0 missed", "");
    let (items, _) = super::items(&s, &[agent("a1", "P", "", Path::new(""), false)], &no_gh(), dl()).unwrap();
    assert!(items.is_empty(), "{items:?}");
    // A running agent is never marked.
    let (items, _) = super::items(&s, &[agent("a1", "P", "", Path::new(""), true)], &no_gh(), dl()).unwrap();
    assert_eq!(items, ["P PR #350"]);
}

#[test]
fn a_hand_back_or_a_handoff_takes_a_finished_agent_off_the_line() {
    let d = fresh_dir("handback");
    let mut s = store(&d);
    put(&mut s, "journal", "note", "H", "2026-11-18T11:00:00.000Z", "waiting: linux-test", "");
    put(&mut s, "journal", "note", "H", "2026-11-18T12:00:00.000Z", "hand-back: PR #360 ready", "");
    let (items, _) = super::items(&s, &[agent("a1", "H", "", Path::new(""), false)], &no_gh(), dl()).unwrap();
    assert!(items.is_empty(), "{items:?}");
    let mut s2 = store(&fresh_dir("handoff"));
    put(&mut s2, "journal", "note", "H", "2026-11-18T11:00:00.000Z", "waiting: linux-test", "");
    put(&mut s2, "handoff", "", "H", "2026-11-18T12:00:00.000Z", "PR fix builder: state, steps left", "");
    let (items, _) = super::items(&s2, &[agent("a1", "H", "", Path::new(""), false)], &no_gh(), dl()).unwrap();
    assert!(items.is_empty(), "{items:?}");
}

#[test]
fn git_head_reads_loose_and_packed_refs_and_a_detached_head() {
    let d = fresh_dir("githead");
    let wt = worktree(&d, "w", "feature/x", SHA);
    assert_eq!(git_head(&wt), Some(("feature/x".to_string(), SHA.to_string())));
    let loose = d.join("main.git").join("refs").join("heads").join("feature");
    std::fs::create_dir_all(&loose).unwrap();
    std::fs::write(loose.join("x"), "1111111111111111111111111111111111111111\n").unwrap();
    assert_eq!(git_head(&wt).unwrap().1, "1111111111111111111111111111111111111111");
    std::fs::write(d.join("main.git").join("worktrees").join("w").join("HEAD"), format!("{SHA}\n")).unwrap();
    assert_eq!(git_head(&wt), Some((String::new(), SHA.to_string())));
    assert_eq!(git_head(&d.join("nowhere")), None);
}

#[test]
fn gh_answers_are_read() {
    assert_eq!(parse_pr_list("[]\n"), Ok(None));
    assert_eq!(
        parse_pr_list(r#"[{"headRefOid":"abc","isDraft":true,"number":12}]"#),
        Ok(Some(Pr { number: 12, head: "abc".into(), draft: Some(true) }))
    );
    assert_eq!(gh_error("To get started with GitHub CLI, please run:  gh auth login"), GhFail::NoAuth);
    assert_eq!(gh_error("error connecting to api.github.com"), GhFail::Failed("no network".into()));
    assert_eq!(Pr { number: 7, head: String::new(), draft: None }.show(), "PR #7");
}

/// #331 review S1: on a shared lane an entry counts for an agent only when
/// it names the agent's worktree or branch.
#[test]
fn on_a_shared_lane_only_an_agents_own_entries_decide_its_mark() {
    let d = fresh_dir("shared");
    let mut s = store(&d);
    let agents = [
        agent("a1", "L", "w1", Path::new(""), false),
        agent("a2", "L", "w2", Path::new(""), false),
        agent("a3", "L", "w3", Path::new(""), false),
    ];
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "waiting: linux-test (w1)", "");
    // a bare wait names nobody: on a shared lane it parks no one.
    put(&mut s, "journal", "note", "L", "2026-11-18T11:05:00.000Z", "waiting: mutants", "");
    put(&mut s, "journal", "note", "L", "2026-11-18T11:10:00.000Z", "waiting: clippy (D:/work/trees/w2)", "");
    let run = |s: &MemoryStore| super::items(s, &agents, &no_gh(), dl()).unwrap().0;
    assert_eq!(run(&s), ["L/w1 parked (own job running)", "L/w2 parked (own job running)"]);
    // a2 hands back: a2 leaves, a1 stays parked.
    put(&mut s, "journal", "note", "L", "2026-11-18T12:00:00.000Z", "hand-back: pr:332 head:abc1234 worktree:w2", "");
    assert_eq!(run(&s), ["L/w1 parked (own job running)"]);
    // A newer entry that names nobody (a reviewer's verdict) does not unpark it.
    put(&mut s, "journal", "note", "L", "2026-11-18T12:30:00.000Z", "#329 r2 merge with follow-ups", "");
    assert_eq!(run(&s), ["L/w1 parked (own job running)"]);
    // Its own later entry does.
    put(&mut s, "journal", "note", "L", "2026-11-18T13:00:00.000Z", "hand-back: done", "worktree: w1");
    assert!(run(&s).is_empty());
}

/// #331 review S2: at most six agents, running ones first, then `+N more`.
#[test]
fn the_agents_line_holds_six_running_first_then_a_count() {
    let d = fresh_dir("cap");
    let mut s = store(&d);
    put(&mut s, "journal", "note", "P", "2026-11-18T11:00:00.000Z", "waiting: linux-test", "");
    let mut agents = vec![agent("p", "P", "", Path::new(""), false)];
    agents.extend((0..8).map(|i| agent(&format!("a{i}"), &format!("L{i}"), "", Path::new(""), true)));
    let (items, _) = super::items(&s, &agents, &no_gh(), dl()).unwrap();
    assert_eq!(items, ["L0", "L1", "L2", "L3", "L4", "L5", "+3 more"]);
}

/// A linked worktree in its own clone under `root`, whose origin is `url`.
fn worktree_of(root: &Path, url: &str, name: &str, branch: &str) -> PathBuf {
    std::fs::create_dir_all(root.join("main.git")).unwrap();
    std::fs::write(root.join("main.git").join("config"), format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {url}\n")).unwrap();
    worktree(root, name, branch, SHA)
}

/// #331 review S3: gh is asked per (repo, branch).
#[test]
fn two_repos_with_one_branch_name_are_asked_apart() {
    let d = fresh_dir("repos");
    let s = store(&d);
    let w1 = worktree_of(&d.join("r1"), "https://github.com/o/ridge.git", "w1", "fix-hooks");
    let w2 = worktree_of(&d.join("r2"), "git@github.com:o/atlas.git", "w2", "fix-hooks");
    assert_eq!((git_repo(&w1), git_repo(&w2)), ("o/ridge".to_string(), "o/atlas".to_string()));
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let gh: Arc<GhList> = Arc::new(move |dir: &Path, _: &str, _| {
        c.fetch_add(1, Ordering::Relaxed);
        let n = if dir.ends_with("w1") { 1 } else { 2 };
        Ok(Some(Pr { number: n, head: String::new(), draft: None }))
    });
    let agents = [agent("a1", "L", "w1", &w1, true), agent("a2", "L", "w2", &w2, true)];
    let (items, _) = super::items(&s, &agents, &gh, dl()).unwrap();
    assert_eq!(items, ["L/w1 PR #1", "L/w2 PR #2"]);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

/// #331 review S5: the caller's deadline bounds the gh calls.
#[test]
fn the_callers_deadline_is_honoured() {
    let d = fresh_dir("deadline");
    let s = store(&d);
    let wt = worktree(&d, "w", "b", SHA);
    let slow: Arc<GhList> = Arc::new(|_: &Path, _: &str, _| {
        std::thread::sleep(Duration::from_secs(3));
        Ok(None)
    });
    let t = Instant::now();
    let (items, note) = super::items(&s, &[agent("a1", "L", "w", &wt, true)], &slow, Instant::now() + Duration::from_millis(200)).unwrap();
    assert!(t.elapsed() < Duration::from_millis(900), "{:?}", t.elapsed());
    assert_eq!((items, note), (vec!["L".to_string()], "gh over 0.2 s".to_string()));
}

/// #331 review N1: a PR the journal or a PR event says is merged or closed
/// is not shown.
#[test]
fn a_merged_or_closed_pr_from_the_journal_is_not_shown() {
    let d = fresh_dir("merged");
    let mut s = store(&d);
    let a = [agent("a1", "L", "", Path::new(""), true)];
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "pr:400 head:4000000", "");
    assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L PR #400 @4000000"]);
    put(&mut s, "journal", "note", "L", "2026-11-18T11:30:00.000Z", "pr:400 merged", "");
    assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L"]);
    put(&mut s, "journal", "note", "L", "2026-11-18T12:00:00.000Z", "pr:401 head:4010000", "");
    assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L PR #401 @4010000"]);
    put(
        &mut s,
        "journal",
        "note",
        "pr-events",
        "2026-11-18T12:30:00.000Z",
        "PR #401 closed",
        "event: pull_request\nrepo: o/a\npr: 401\nhead: 4010000\nverdict: closed",
    );
    assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L"]);
}

/// #331 review N4: the hooks' default gh in a test build never spawns gh;
/// a branch that is not a plain name is never given to gh.
#[test]
fn the_default_gh_in_tests_spawns_nothing_and_odd_branches_are_refused() {
    let nowhere = std::env::temp_dir().join("atlas-card-agents-no-such-dir");
    assert_eq!(default_gh()(&nowhere, "main", Instant::now() + GH_BUDGET), Ok(None));
    assert!(plain_branch("feature/x-1.2_y+z") && !plain_branch("--repo=x") && !plain_branch("a b") && !plain_branch(""));
    assert_eq!(gh_pr_list(&nowhere, "-x", Instant::now()), Err(GhFail::Failed("branch name not plain".into())));
}

/// An agent like `agent`, launched at `launched`.
fn agent_at(id: &str, lane: &str, wt: &str, launched: &str, running: bool) -> Agent {
    Agent { launched: launched.into(), ..agent(id, lane, wt, Path::new(""), running) }
}

/// #331 r2 S1: two agents in turn on one worktree. An entry tied by the
/// worktree is the newest agent's launched before it, and the line names
/// the worktree once.
#[test]
fn a_reused_worktree_ties_its_entries_to_the_newest_agent_launched_on_it() {
    let d = fresh_dir("reuse");
    let mut s = store(&d);
    let second = "2026-11-18T12:00:00.000Z";
    let run = |s: &MemoryStore, a: &[Agent]| super::items(s, a, &no_gh(), dl()).unwrap().0;
    // The first builder hands back; the second, on the same worktree, waits.
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "hand-back: done (w)", "");
    put(&mut s, "journal", "note", "L", "2026-11-18T12:30:00.000Z", "waiting: linux-test (w)", "");
    let both = [agent_at("a1", "L", "w", LAUNCH, false), agent_at("a2", "L", "w", second, false)];
    assert_eq!(run(&s, &both), ["L/w parked (own job running)"]);
    // The second hands back by its id: the first is not parked by the
    // second's wait.
    put(&mut s, "journal", "note", "L", "2026-11-18T13:00:00.000Z", "hand-back: done", "agent: a2");
    assert!(run(&s, &both).is_empty(), "{:?}", run(&s, &both));
    // A first builder stopped mid-wait and a second one waiting: one row.
    let mut s2 = store(&fresh_dir("reuse2"));
    put(&mut s2, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "waiting: mutants (w)", "");
    put(&mut s2, "journal", "note", "L", "2026-11-18T12:30:00.000Z", "waiting: linux-test (w)", "");
    assert_eq!(run(&s2, &both), ["L/w parked (own job running)"]);
    // Both running: one row too.
    let running = [agent_at("a1", "L", "w", LAUNCH, true), agent_at("a2", "L", "w", second, true)];
    assert_eq!(run(&s2, &running), ["L/w"]);
}

/// #331 r2 S2: `pr:` in each form the journal tool takes, and a merged
/// note in the repo-qualified form hides the PR.
#[test]
fn a_pr_entry_is_read_in_every_form_the_journal_takes() {
    for (i, form) in ["pr:o/atlas#331", "pr:atlas#331", "pr:#331", "pr:331", "pr: #331", "pr: o/atlas#331"].iter().enumerate() {
        let mut s = store(&fresh_dir(&format!("forms{i}")));
        put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", &format!("{form} head:b7d5a72"), "");
        let a = [agent("a1", "L", "", Path::new(""), true)];
        assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L PR #331 @b7d5a72"], "{form}");
        put(&mut s, "journal", "note", "L", "2026-11-18T12:00:00.000Z", &format!("{form} merged"), "");
        assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L"], "{form} merged");
    }
    let mut s = store(&fresh_dir("formsmeta"));
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "opened", "pr: o/atlas#332\nhead: 3333333");
    assert_eq!(super::items(&s, &[agent("a1", "L", "", Path::new(""), true)], &no_gh(), dl()).unwrap().0, ["L PR #332 @3333333"]);
    // A PR of another repo than the agent's worktree is not its PR.
    let d = fresh_dir("formsrepo");
    let mut s = store(&d);
    let wt = worktree_of(&d, "https://github.com/o/atlas.git", "w", "b");
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "pr:o/ridge#9 head:9999999", "");
    let calls = Arc::new(AtomicUsize::new(0));
    let (items, _) = super::items(&s, &[agent("a1", "L", "w", &wt, true)], &gh_for("b", 10, calls), dl()).unwrap();
    assert_eq!(items, ["L PR #10 @abcdef0 (ready)"]);
    assert_eq!(pr_ref("o/atlas#331"), Some((331, "o/atlas".into())));
    assert!(same_repo("atlas", "o/atlas") && same_repo("O/Atlas", "o/atlas") && !same_repo("ridge", "o/atlas") && same_repo("x", ""));
}

/// #331 r2 N1, N2: `waiting: worktree:X` and `(worktree:X)` tie; a branch
/// named like a common word ties only as `branch:`.
#[test]
fn worktree_words_tie_in_any_spelling_and_a_branch_only_as_branch() {
    let d = fresh_dir("spell");
    let wt = worktree(&d, "w1", "main", SHA);
    let agents = [agent("a1", "L", "w1", &wt, false), agent("a2", "L", "w2", Path::new(""), false)];
    // w1's branch has no PR.
    let quiet = gh_for("-", 1, Arc::new(AtomicUsize::new(0)));
    for line in [
        "waiting: worktree:w1 linux-test",
        "waiting: linux-test (worktree:w1)",
        "waiting: linux-test worktree:w1)",
        "waiting: linux-test (worktree:w1, branch:fix)",
    ] {
        let mut s = store(&fresh_dir("spell1"));
        put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", line, "");
        assert_eq!(super::items(&s, &agents, &quiet, dl()).unwrap().0, ["L/w1 parked (own job running)"], "{line}");
    }
    let mut s = store(&fresh_dir("spell2"));
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "waiting: linux-test (w1)", "");
    put(&mut s, "journal", "note", "L", "2026-11-18T11:30:00.000Z", "#329 merged to main", "");
    assert_eq!(super::items(&s, &agents, &quiet, dl()).unwrap().0, ["L/w1 parked (own job running)"]);
    put(&mut s, "journal", "note", "L", "2026-11-18T12:00:00.000Z", "hand-back: done branch:main", "");
    assert!(super::items(&s, &agents, &quiet, dl()).unwrap().0.is_empty());
}

/// #331 r3 S1: a hand-back line does not close its own PR. A merged or
/// closed word counts only right after the `pr:` value, or as the value of
/// `state:`/`verdict:`, and never after "not".
#[test]
fn a_hand_back_line_does_not_close_its_own_pr() {
    let a = [agent("a1", "L", "", Path::new(""), true)];
    for (i, line) in [
        "pr:o/atlas#331 head:5f24582 -- hand-back fix2: S1 S2, gates green. Not merged.",
        "pr:o/atlas#331 head:5f24582 -- merged origin/main, gates green",
        "pr: #331 not merged head:5f24582",
        "pr:331 head:5f24582 verdict: not merged",
    ]
    .iter()
    .enumerate()
    {
        let mut s = store(&fresh_dir(&format!("handback{i}")));
        put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", line, "");
        assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L PR #331 @5f24582"], "{line}");
        put(&mut s, "journal", "note", "L", "2026-11-18T12:00:00.000Z", "pr:o/atlas#331 merged", "");
        assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L"], "{line}");
    }
    for (i, closing) in ["pr: o/atlas#331 closed.", "pr:331 head:5f24582 state:merged", "pr:331 verdict: closed"].iter().enumerate() {
        let mut s = store(&fresh_dir(&format!("closing{i}")));
        put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", closing, "");
        assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L"], "{closing}");
    }
}

/// #331 r3 N3: the one-line meta `pr: o/r#N head:SHA` parses.
#[test]
fn a_one_line_pr_meta_parses() {
    let a = [agent("a1", "L", "", Path::new(""), true)];
    let mut s = store(&fresh_dir("onelinemeta"));
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "opened", "pr: o/atlas#332 head:3333333");
    assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L PR #332 @3333333"]);
    put(&mut s, "journal", "note", "L", "2026-11-18T12:00:00.000Z", "done", "pr: o/atlas#332 merged");
    assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L"]);
}

/// #331 r3 N1: an earlier agent stopped mid-wait stays off the line once
/// the later agent on its worktree finishes (one worktree per agent).
#[test]
fn an_earlier_agent_stopped_mid_wait_stays_off_after_the_later_one_finishes() {
    let mut s = store(&fresh_dir("reuse3"));
    put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", "waiting: mutants (w)", "");
    put(&mut s, "journal", "note", "L", "2026-11-18T12:30:00.000Z", "hand-back: done (w)", "");
    let both = [agent_at("a1", "L", "w", LAUNCH, false), agent_at("a2", "L", "w", "2026-11-18T12:00:00.000Z", false)];
    let items = super::items(&s, &both, &no_gh(), dl()).unwrap().0;
    assert!(items.is_empty(), "{items:?}");
}

/// #331 r2 S5: gh's deadline never passes the hook's own, and once it has
/// passed gh is not spawned (a missing directory would fail the spawn).
#[test]
fn the_gh_deadline_is_cut_at_the_hooks_and_a_spent_one_spawns_nothing() {
    let now = Instant::now();
    let hook = now + Duration::from_millis(200);
    assert_eq!(gh_deadline(now, hook), hook);
    assert_eq!(gh_deadline(now, now + Duration::from_secs(10)), now + GH_BUDGET);
    let nowhere = std::env::temp_dir().join("atlas-card-agents-no-such-dir");
    assert_eq!(gh_pr_list(&nowhere, "main", now), Err(GhFail::Timeout));
}

/// #331 r4 S1: a merge note in the house form closes its PR; the
/// `key:value` words after the `pr:` value are skipped. (text, meta)
#[test]
fn a_house_form_merge_note_closes_its_pr() {
    let a = [agent("a1", "L", "", Path::new(""), true)];
    let open = ("pr:o/atlas#331 head:5f24582 -- hand-back", "");
    for (i, (text, meta)) in [
        ("pr:o/atlas#331 head:abc1234 -- merged as def5678", ""),
        ("pr:331 head:abc1234 merged", ""),
        ("done", "pr: o/atlas#331 head:abc1234 merged"),
        ("Merged.", "pr: o/atlas#331\nhead: abc1234"),
        ("Merged #331 as def5678", "pr: o/atlas#331"),
    ]
    .iter()
    .enumerate()
    {
        let mut s = store(&fresh_dir(&format!("housemerge{i}")));
        put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", open.0, open.1);
        assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L PR #331 @5f24582"], "{text}");
        put(&mut s, "journal", "note", "L", "2026-11-18T12:00:00.000Z", text, meta);
        assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L"], "{text} | {meta}");
    }
    for (i, (text, meta)) in [
        ("Not merged.", "pr: o/atlas#331\nhead: 5f24582"),
        ("merged origin/main into the branch", "pr: o/atlas#331\nhead: 5f24582"),
        ("pr:o/atlas#331 head:5f24582 -- merged origin/main, gates green", ""),
        ("pr:331 head:5f24582 verdict: not merged", ""),
        ("pr:o/atlas#331 head:5f24582 -- hand-back fix2. Not merged.", ""),
    ]
    .iter()
    .enumerate()
    {
        let mut s = store(&fresh_dir(&format!("housekeep{i}")));
        put(&mut s, "journal", "note", "L", "2026-11-18T11:00:00.000Z", text, meta);
        assert_eq!(super::items(&s, &a, &no_gh(), dl()).unwrap().0, ["L PR #331 @5f24582"], "{text} | {meta}");
    }
}

/// #331 r4 nit 2: with the hook's budget spent before the lookup, the note
/// says gh was skipped, not that it ran over 0.0 s.
#[test]
fn a_spent_budget_says_gh_was_skipped() {
    let d = fresh_dir("spent");
    let s = store(&d);
    let wt = worktree(&d, "w", "b", SHA);
    // As gh_pr_list answers past its deadline.
    let spent: Arc<GhList> = Arc::new(|_: &Path, _: &str, _| Err(GhFail::Timeout));
    let (items, note) = super::items(&s, &[agent("a1", "L", "w", &wt, true)], &spent, Instant::now()).unwrap();
    assert_eq!((items, note), (vec!["L".to_string()], "gh skipped: hook budget spent".to_string()));
}
