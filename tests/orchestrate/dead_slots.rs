//! #389: a held cap slot whose agent died in a reboot, or with its Claude
//! Code process, counts as released on that positive evidence only, and
//! `atlas queue release` takes a slot number, agent id or lane. Part of
//! `tests/orchestrate.rs` (its helpers).

use super::*;
use atlas::orchestrate::{dead_holder, DeadReason, HeldSlot};
use atlas::queue::{held_slots_at, held_text, release_target, DeadProbe};
use std::time::{Duration, SystemTime};

const HOUR: Duration = Duration::from_secs(3600);

/// The decision alone: a boot after the last write, or an owner process
/// known gone, releases; anything unknown keeps the slot.
#[test]
fn the_decision_releases_on_positive_evidence_only() {
    let last = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    assert_eq!(dead_holder(last, Some(last + HOUR), None), Some(DeadReason::Rebooted));
    assert_eq!(dead_holder(last, Some(last + HOUR), Some(true)), Some(DeadReason::Rebooted));
    assert_eq!(dead_holder(last, None, Some(false)), Some(DeadReason::OwnerGone));
    assert_eq!(dead_holder(last, Some(last - HOUR), Some(false)), Some(DeadReason::OwnerGone));
    // Live, or no evidence: kept.
    assert_eq!(dead_holder(last, Some(last - HOUR), Some(true)), None);
    assert_eq!(dead_holder(last, None, None), None);
    // A boot read a hair after the last write is within the clock slack.
    assert_eq!(dead_holder(last, Some(last + Duration::from_secs(2)), None), None);
}

/// A session with builders `ids` running in lane `lane` (all written now),
/// each with a launch entry naming `owner`, and its store.
pub(super) fn running(t: &Temp, ids: &[&str], lanes: &[&str], owner: &str) -> (Session, atlas::memory::MemoryStore, OrchestrateSpec) {
    let s = Session::new(t.path());
    let spec = builders(4);
    let set = settings(t.path(), &spec, &[]);
    let mut store = atlas::memory::MemoryStore::open(&set.store).unwrap();
    file_launches(&mut store, &s, ids, lanes, owner);
    (s, store, spec)
}

/// Builders `ids` running in `lanes` in session `s`, each with a launch
/// entry as the SubagentStart hook files it: slot numbers 1, 2, ... in
/// order, and an `owner:` line unless `owner` is empty.
pub(super) fn file_launches(store: &mut atlas::memory::MemoryStore, s: &Session, ids: &[&str], lanes: &[&str], owner: &str) {
    for (n, (id, lane)) in ids.iter().zip(lanes).enumerate() {
        let brief = format!("Kind: builder\nLane: {lane}\n\nBuild {id}.");
        s.agent(id, "general-purpose", &format!("t{id}"), &brief, 1000, false);
        let owner = if owner.is_empty() { String::new() } else { format!("owner: {owner}\n") };
        let d = atlas::memory::Draft {
            title: format!("launch: job {id}"),
            meta: format!(
                "agent: {id}\ntype: general-purpose\nmodel: opus\ndescription: job {id}\nstart: 2026-11-18T00:00:00.000Z\nsession: s1\ntranscript: {}\nserver: 1 -\n{owner}slot: {}\n",
                s.tp(),
                n + 1
            ),
            body: brief,
            reason: "test".into(),
            author: "hook".into(),
            client: "atlas-hook".into(),
            ..atlas::memory::Draft::default()
        };
        let mut w = store.writer().unwrap();
        w.create(&format!("journal:{lane}:{:04}", n + 1), "journal", atlas::hooks::LAUNCH_ENTRY, lane, &d, None).unwrap();
        w.commit().unwrap();
    }
}

pub(super) fn orch(t: &Temp, spec: &OrchestrateSpec) -> atlas::orchestrate::OrchSettings {
    settings(t.path(), spec, &[]).orchestrate
}

pub(super) fn slots(o: &atlas::orchestrate::OrchSettings, store: &atlas::memory::MemoryStore, probe: &DeadProbe<'_>) -> Vec<(Option<PathBuf>, HeldSlot)> {
    held_slots_at(o, store, SystemTime::now(), probe).unwrap()
}

/// The issue: the machine rebooted after the agent last wrote. Its slot is
/// listed released, with the reason, and holds nothing.
#[test]
fn a_reboot_after_the_last_write_releases_the_slot() {
    let t = Temp::new("orch-389-reboot");
    let (_s, store, spec) = running(&t, &["b1"], &["WALRUS"], "4242 7");
    let o = orch(&t, &spec);
    let unknown = |_: &str| None;
    let probe = DeadProbe { boot: Some(SystemTime::now() + HOUR), owner_alive: &unknown };
    let held = slots(&o, &store, &probe);
    assert!(held.len() == 1 && held[0].1.released == Some(DeadReason::Rebooted) && held[0].1.agent == "b1", "{held:?}");
    let text = held_text(&held.into_iter().map(|(_, h)| h).collect::<Vec<_>>(), &atlas::memory::now());
    assert!(text.starts_with("  #1  released: boot after last write  builder  lane WALRUS"), "{text}");
    assert!(!text.contains("held, oldest first"), "{text}");
}

/// The owning Claude Code process is gone (its tag probes dead): released.
#[test]
fn a_gone_owner_releases_the_slot() {
    let t = Temp::new("orch-389-owner");
    let (_s, store, spec) = running(&t, &["b1"], &["WALRUS"], "4242 7");
    let o = orch(&t, &spec);
    let gone = |tag: &str| (tag == "4242 7").then_some(false);
    let probe = DeadProbe { boot: Some(SystemTime::now() - HOUR), owner_alive: &gone };
    let held = slots(&o, &store, &probe);
    assert!(held.len() == 1 && held[0].1.released == Some(DeadReason::OwnerGone), "{held:?}");
}

/// A live owner and an older boot, or no evidence at all: the slot stays
/// held and numbered.
#[test]
fn a_live_or_unknown_owner_keeps_the_slot() {
    let t = Temp::new("orch-389-live");
    let (_s, store, spec) = running(&t, &["b1"], &["WALRUS"], "4242 7");
    let o = orch(&t, &spec);
    let live = |_: &str| Some(true);
    let unknown = |_: &str| None;
    for probe in [
        DeadProbe { boot: Some(SystemTime::now() - HOUR), owner_alive: &live },
        DeadProbe { boot: None, owner_alive: &unknown },
    ] {
        let held = slots(&o, &store, &probe);
        assert!(held.len() == 1 && held[0].1.released.is_none() && held[0].1.running, "{held:?}");
        let text = held_text(&[held[0].1.clone()], &atlas::memory::now());
        assert!(text.starts_with("1 agent cap slot is held") && text.contains("  #1  builder  lane WALRUS"), "{text}");
    }
}

/// `atlas queue release` by slot number, agent id or lane resolves to one
/// agent, and its release frees the slot.
#[test]
fn a_manual_release_by_slot_id_or_lane_frees_it() {
    let t = Temp::new("orch-389-manual");
    let (s, store, spec) = running(&t, &["b1", "b2"], &["WALRUS", "OTTER"], "4242 7");
    let o = orch(&t, &spec);
    let unknown = |_: &str| None;
    let probe = DeadProbe { boot: None, owner_alive: &unknown };
    let held = slots(&o, &store, &probe);
    assert_eq!(held.len(), 2, "{held:?}");
    let pick = |sel: &str| release_target(sel, &held).unwrap().map(|(d, a)| (d.to_path_buf(), a.to_string()));
    let first = held[0].1.agent.clone();
    assert_eq!(pick("1").map(|p| p.1), Some(first.clone()));
    assert_eq!(pick("#1").map(|p| p.1), Some(first));
    assert_eq!(pick("agent-b2"), Some((s.dir.clone(), "b2".to_string())));
    assert_eq!(pick("otter").map(|p| p.1).as_deref(), Some("b2"));
    assert_eq!(pick("NOSUCH"), None, "an unknown selector falls back to the on-disk agent lookup");
    assert!(release_target("3", &held).unwrap_err().contains("no slot #3"));
    // The release itself (forced: the agent wrote just now).
    let (dir, id) = pick("OTTER").unwrap();
    let said = atlas::orchestrate::release(&store, &dir, &id, true, SystemTime::now()).unwrap();
    assert!(said.starts_with("released agent b2 "), "{said}");
    let after = slots(&o, &store, &probe);
    assert!(after.len() == 1 && after[0].1.agent == "b1", "{after:?}");
}

/// A lane two running agents hold names neither: refused, both listed.
#[test]
fn an_ambiguous_selector_is_refused() {
    let t = Temp::new("orch-389-ambiguous");
    let (_s, store, spec) = running(&t, &["b1", "b2"], &["WALRUS", "WALRUS"], "4242 7");
    let o = orch(&t, &spec);
    let unknown = |_: &str| None;
    let held = slots(&o, &store, &DeadProbe { boot: None, owner_alive: &unknown });
    let e = release_target("WALRUS", &held).unwrap_err();
    assert!(e.contains("names 2 held slots") && e.contains("agent b1") && e.contains("agent b2"), "{e}");
    // A pending launch carries no number, and its lane names nothing to
    // release.
    let pending =
        HeldSlot { kind: Kind::Builder, lane: "X".into(), description: "d".into(), since: String::new(), running: false, agent: String::new(), released: None, slot: None };
    let e = release_target("1", &[(None, pending.clone())]).unwrap_err();
    assert!(e.contains("no slot #1"), "{e}");
    assert_eq!(release_target("X", &[(None, pending)]).unwrap(), None);
}

/// The resident server names its owner (the Claude Code process above it,
/// when one provably is) on each launch entry, beside its own tag, and
/// numbers each launch: 1, then 2.
#[test]
fn a_launch_entry_names_its_owner_process_and_slot() {
    let t = Temp::new("orch-389-owner-line");
    let s = Session::new(t.path());
    let mut h = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    h.enable_restart_point();
    let brief = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", brief).deny.is_none());
    s.agent("b1", "general-purpose", "t1", brief, 1000, false);
    let _ = start(&mut h, &s, "b1");
    let entry = entries(&h, "WALRUS", "launch")[0].clone();
    assert!(entry.meta.contains("\nslot: 1\n"), "{}", entry.meta);
    // Under a test runner the parent is cargo, not Claude Code: no owner
    // line. Under Claude Code itself, the line names it, and it runs.
    match atlas::restart::owner_tag() {
        Some(owner) => {
            assert!(entry.meta.contains(&format!("owner: {owner}\n")), "{}", entry.meta);
            assert_eq!(atlas::restart::server_live(&owner), Some(true));
        }
        None => assert!(!entry.meta.contains("owner:"), "{}", entry.meta),
    }
    let brief2 = "Kind: builder\nLane: OTTER\n\nBuild the otter parser.";
    assert!(launch_with(&mut h, &s, "t2", brief2).deny.is_none());
    s.agent("b2", "general-purpose", "t2", brief2, 1000, false);
    let _ = start(&mut h, &s, "b2");
    let entry = entries(&h, "OTTER", "launch")[0].clone();
    assert!(entry.meta.contains("\nslot: 2\n"), "{}", entry.meta);
}
