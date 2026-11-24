//! #351 F1: the hook's worktree gc tick runs after recall, in a skippable
//! budget phase of its own (`worktree_gc`).

use super::*;
use std::time::{Duration, Instant};

/// Hooks over a fresh temp store, orchestrated, with a hook budget of `ms`.
fn hooks(name: &str, ms: u64) -> (PathBuf, Hooks) {
    let dir = std::env::temp_dir().join(format!("atlas-hook-gcphase-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let vault = dir.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let db = dir.join("vaults.sqlite");
    drop(crate::store::Store::open(&db, crate::paths::Vaults::new(&[("v".into(), vault)]).unwrap()).unwrap());
    let mut s = Settings::from_config(None, &db).unwrap();
    s.snapshot_secs = 0;
    s.orchestrate.enabled = true;
    s.hook_budget = Duration::from_millis(ms);
    (dir, Hooks::open(s).unwrap())
}

fn names(h: &Hooks) -> Vec<&'static str> {
    h.budget.phases().iter().map(|p| p.0).collect()
}

#[test]
fn a_spent_budget_skips_the_gc_tick_and_the_next_event_retries() {
    if !crate::hook_budget::enforced() {
        eprintln!("skipped: {} is set", crate::hook_budget::OFF_VAR);
        return;
    }
    let (dir, mut h) = hooks("spent", 1_000);
    let stamp = crate::worktree_gc::stamp_path(h.store().path());
    let ev = HookEvent { event: "SubagentStop".into(), session_id: "s1".into(), agent_id: "a1".into(), ..HookEvent::default() };
    // The event waited past its whole budget in the queue: the tick is
    // skipped and named, and no stamp is written.
    let a = h.handle_from(&ev, Instant::now() - Duration::from_secs(5));
    assert!(h.budget.skipped().contains(&"worktree_gc"), "{:?}", h.budget.skipped());
    // #355 S2: in the trace, never in the model's context, and SubagentStop
    // answers as it did before the tick had a phase: nothing.
    assert!(h.budget.trace_line(&ev.event).contains("skipped=worktree_gc"), "{}", h.budget.trace_line(&ev.event));
    assert!(!a.context.contains("worktree_gc"), "{a:?}");
    assert_eq!(a.to_json(&ev.event), HookAnswer::default().to_json(&ev.event), "{a:?}");
    assert!(!stamp.exists(), "a skipped tick wrote its stamp");
    // The next event, in time, runs it (an hour is due: no stamp yet).
    h.settings.hook_budget = Duration::from_secs(60);
    h.handle(&ev);
    assert!(names(&h).contains(&"worktree_gc"), "{:?}", names(&h));
    assert!(stamp.exists(), "the retried tick wrote no stamp");
    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_gc_tick_runs_after_recall_so_it_cannot_spend_recalls_budget() {
    let (dir, mut h) = hooks("order", 60_000);
    let ev = HookEvent { event: "UserPromptSubmit".into(), session_id: "s1".into(), prompt: "what is the widget cache".into(), ..HookEvent::default() };
    h.handle(&ev);
    let p = names(&h);
    let at = |n: &str| p.iter().position(|x| *x == n).unwrap_or_else(|| panic!("no {n} phase in {p:?}"));
    assert!(at("recall") < at("worktree_gc"), "{p:?}");
    assert!(at("ideas") < at("worktree_gc"), "{p:?}");
    // #355 N3: the PR notice and the log row are timed apart from gc.
    assert!(at("worktree_gc") < at("notice"), "{p:?}");
    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #355 N4: the SessionStart line comes from the stored summary, so a spent
/// budget that skips the tick still shows it.
#[test]
fn a_spent_budget_keeps_the_session_start_notify_line() {
    if !crate::hook_budget::enforced() {
        eprintln!("skipped: {} is set", crate::hook_budget::OFF_VAR);
        return;
    }
    let (dir, mut h) = hooks("notify", 1_000);
    h.settings.worktrees.auto_gc = false;
    h.settings.worktrees.notify_bytes = 1;
    std::fs::write(crate::worktree_gc::summary_path(h.store().path()), "5000000000 3\n").unwrap();
    let ev = HookEvent { event: "SessionStart".into(), session_id: "s1".into(), source: "startup".into(), ..HookEvent::default() };
    // Past SessionStart's longer budget too (#333 item 2).
    let a = h.handle_from(&ev, Instant::now() - Duration::from_millis(crate::hook_budget::LONG_MS + 1_000));
    assert!(h.budget.skipped().contains(&"worktree_gc"), "{:?}", h.budget.skipped());
    assert!(a.context.contains("3 stale worktrees hold"), "{a:?}");
    assert!(!crate::worktree_gc::stamp_path(h.store().path()).exists(), "a skipped tick wrote its stamp");
    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #355 N2: an event whose own work fails still runs its gc tick once.
#[test]
fn an_event_whose_work_fails_still_ticks() {
    let (dir, mut h) = hooks("fail", 60_000);
    // No records table: the lane read of an agent report fails.
    h.store().conn().execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE records;").unwrap();
    let prompt = "<agent-message from=\"x\">\nLane: GCFAIL\n</agent-message>".to_string();
    let ev = HookEvent { event: "UserPromptSubmit".into(), session_id: "s1".into(), prompt, ..HookEvent::default() };
    let a = h.handle(&ev);
    assert!(a.context.is_empty(), "{a:?}");
    let p = names(&h);
    assert_eq!(p.iter().filter(|x| **x == "worktree_gc").count(), 1, "a failed event did not tick once: {p:?}");
    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}
