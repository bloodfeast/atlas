//! #389 review M1/S3/S4/S5: a held slot's number belongs to its launch
//! (the `slot:` line its launch entry carries), so it is the same in every
//! view (a `--lane` listing, after another agent finishes) and `atlas queue
//! release N` frees the agent the listing showed as `#N`. Part of
//! `tests/orchestrate.rs` (its helpers).

use super::dead_slots::{file_launches, orch, running, slots};
use super::*;
use atlas::queue::{held_text, release_target, DeadProbe};

fn texts(held: &[(Option<PathBuf>, atlas::orchestrate::HeldSlot)], lane: Option<&str>) -> String {
    let rows: Vec<_> = held.iter().map(|(_, h)| h.clone()).filter(|h| lane.is_none_or(|l| atlas::lanes::in_lane(&h.lane, l))).collect();
    held_text(&rows, &atlas::memory::now())
}

/// The numbers are the launches' own: a lane filter keeps them, and a
/// finished agent leaves the others' numbers where they were.
#[test]
fn slot_numbers_survive_a_lane_filter_and_a_finish() {
    let t = Temp::new("orch-389-stable");
    let (s, store, spec) = running(&t, &["b1", "b2"], &["WALRUS", "OTTER"], "");
    let o = orch(&t, &spec);
    let unknown = |_: &str| None;
    let probe = DeadProbe { boot: None, owner_alive: &unknown };
    let held = slots(&o, &store, &probe);
    let otter = texts(&held, Some("OTTER"));
    assert!(otter.contains("  #2  builder  lane OTTER") && !otter.contains("#1"), "{otter}");
    // b1 finishes: b2 keeps #2, and #1 names nothing.
    s.agent("b1", "general-purpose", "tb1", "Kind: builder\nLane: WALRUS\n\nBuild b1.", 1000, true);
    let after = slots(&o, &store, &probe);
    let text = texts(&after, None);
    assert!(text.contains("  #2  builder  lane OTTER") && !text.contains("#1"), "{text}");
    assert_eq!(release_target("2", &after).unwrap().map(|(_, a)| a.to_string()).as_deref(), Some("b2"));
    let e = release_target("1", &after).unwrap_err();
    assert!(e.contains("no slot #1"), "{e}");
}

/// S4: a row listed released (dead on the evidence) is still named by its
/// lane and its number, so `atlas queue release` records it.
#[test]
fn a_released_row_is_named_by_lane_and_number() {
    let t = Temp::new("orch-389-released-sel");
    let (_s, store, spec) = running(&t, &["b1"], &["WALRUS"], "4242 7");
    let o = orch(&t, &spec);
    let gone = |_: &str| Some(false);
    let held = slots(&o, &store, &DeadProbe { boot: None, owner_alive: &gone });
    assert!(held[0].1.released.is_some(), "{held:?}");
    let text = texts(&held, None);
    assert!(text.contains("  #1  released: owning Claude Code process gone  builder  lane WALRUS"), "{text}");
    assert_eq!(release_target("WALRUS", &held).unwrap().map(|(_, a)| a.to_string()).as_deref(), Some("b1"));
    assert_eq!(release_target("1", &held).unwrap().map(|(_, a)| a.to_string()).as_deref(), Some("b1"));
}

/// S5: the worktree gc's view counts an agent on a recorded finish only:
/// an owner the machine probes as gone still holds there.
#[test]
fn the_gc_view_ignores_reboot_and_owner_evidence() {
    let t = Temp::new("orch-389-gc-view");
    let (_s, store, spec) = running(&t, &["b1"], &["WALRUS"], "4294967290 1");
    let o = orch(&t, &spec);
    assert_eq!(atlas::restart::server_live("4294967290 1"), Some(false));
    let held = atlas::queue::held_slots(&o, &store, std::time::SystemTime::now()).unwrap();
    assert!(held.len() == 1 && held[0].released.is_some(), "{held:?}");
    let (hs, read, _) = atlas::queue::launch_holders_counted(&o, &store, std::time::SystemTime::now()).unwrap();
    assert!(read == 1 && hs.len() == 1 && hs[0].name.contains("b1"), "{:?}", hs.iter().map(|h| &h.name).collect::<Vec<_>>());
}

/// The binary: `list --lane` shows the global numbers, and `release` takes
/// a number, a lane, an agent id with `--transcript`, and an agent id with
/// no held slot (looked up on disk).
#[test]
fn the_cli_lists_and_releases_by_slot_number_lane_and_id() {
    let t = Temp::new("orch-389-cli");
    let claude = t.path().join("claude");
    let s = Session::new(&claude.join("projects"));
    let cfg = t.path().join("atlas.json");
    let store_path = t.path().join("memory.sqlite");
    std::fs::write(
        &cfg,
        format!(
            "{{\"db\": {:?}, \"vaults\": [], \"memory\": {{\"store\": {:?}}}, \"orchestrate\": {{\"enabled\": true, \"kinds\": {{\"builder\": {{\"max\": 4}}}}}}}}",
            t.path().join("index.sqlite").display().to_string(),
            store_path.display().to_string()
        ),
    )
    .unwrap();
    {
        let mut store = atlas::memory::MemoryStore::open(&store_path).unwrap();
        file_launches(&mut store, &s, &["b1", "b2", "b3"], &["WALRUS", "OTTER", "SEAL"], "");
    }
    // An agent with no launch entry: no held slot names it.
    s.agent("b9", "general-purpose", "tb9", "Kind: builder\nLane: KELP\n\nBuild b9.", 1000, false);
    let queue = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_atlas"))
            .arg("--config")
            .arg(&cfg)
            .arg("queue")
            .args(args)
            .env("ATLAS_HOME", t.path().join("home-atlas"))
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("HOME", t.path())
            .env("USERPROFILE", t.path())
            .env_remove("ATLAS_CONFIG")
            .env_remove("ATLAS_PROJECT")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let (ok, out, err) = queue(&["list", "--lane", "OTTER"]);
    assert!(ok && out.contains("  #2  builder  lane OTTER") && !out.contains("#1"), "{out}{err}");
    // A number names the slot the listing showed, whatever the filter.
    let (ok, out, err) = queue(&["release", "2", "--force"]);
    assert!(ok && out.starts_with("released agent b2 "), "{out}{err}");
    let (ok, out, err) = queue(&["list"]);
    assert!(ok && out.contains("  #1  builder  lane WALRUS") && out.contains("  #3  builder  lane SEAL") && !out.contains("OTTER"), "{out}{err}");
    // A freed number names nothing now.
    let (ok, out, err) = queue(&["release", "2", "--force"]);
    assert!(!ok && err.contains("no slot #2"), "{out}{err}");
    let (ok, out, err) = queue(&["release", "WALRUS", "--force"]);
    assert!(ok && out.starts_with("released agent b1 "), "{out}{err}");
    // With --transcript the selector is an agent id in that session.
    let (ok, out, err) = queue(&["release", "3", "--transcript", &s.tp(), "--force"]);
    assert!(!ok && err.contains("no agent 3"), "{out}{err}");
    let (ok, out, err) = queue(&["release", "agent-b3", "--transcript", &s.tp(), "--force"]);
    assert!(ok && out.starts_with("released agent b3 "), "{out}{err}");
    // No held slot names b9: its id is looked up on disk.
    let (ok, out, err) = queue(&["release", "b9", "--force"]);
    assert!(ok && out.starts_with("released agent b9 "), "{out}{err}");
}

/// S2: the owner is the Claude Code process above the server, past shells
/// and launchers; anything else is unknown, so no slot is released on it.
#[test]
fn the_owner_is_claude_code_past_wrappers_or_unknown() {
    use atlas::restart::owner_pid;
    let p = |rows: &[(u32, u32, &str)]| rows.iter().map(|(a, b, n)| (*a, *b, (*n).to_string())).collect::<Vec<_>>();
    // Claude Code started the server directly, or through cmd /c, bash -c
    // or the MSYS stub.
    assert_eq!(owner_pid(&p(&[(1, 0, "init"), (50, 1, "claude"), (60, 50, "atlas")]), 60, |_| None), Some(50));
    assert_eq!(owner_pid(&p(&[(50, 4, "node.exe"), (55, 50, "cmd.exe"), (60, 55, "atlas.exe")]), 60, |_| None), Some(50));
    assert_eq!(owner_pid(&p(&[(50, 4, "Claude.EXE"), (52, 50, "bash.exe"), (55, 52, "bash.exe"), (60, 55, "atlas.exe")]), 60, |_| None), Some(50));
    // Unknown parent, init, a parent gone from the table, a loop: unknown.
    assert_eq!(owner_pid(&p(&[(50, 4, "cargo"), (60, 50, "atlas")]), 60, |_| None), None);
    assert_eq!(owner_pid(&p(&[(55, 1, "sh"), (60, 55, "atlas")]), 60, |_| None), None);
    assert_eq!(owner_pid(&p(&[(60, 55, "atlas")]), 60, |_| None), None);
    assert_eq!(owner_pid(&p(&[(55, 56, "sh"), (56, 55, "bash"), (60, 55, "atlas")]), 60, |_| None), None);
    assert_eq!(owner_pid(&[], 60, |_| None), None);
}

/// #395: a `node` or `npx` shim between Claude Code and the server is not
/// the owner (the topmost Claude Code-named ancestor is), and a parent pid
/// that started after its child was reused, so the walk stops below it.
#[test]
fn the_owner_skips_a_shim_and_a_reused_parent_pid() {
    use atlas::restart::owner_pid;
    let p = |rows: &[(u32, u32, &str)]| rows.iter().map(|(a, b, n)| (*a, *b, (*n).to_string())).collect::<Vec<_>>();
    let at = |starts: &'static [(u32, u64)]| move |pid: u32| starts.iter().find(|s| s.0 == pid).map(|s| s.1);
    // Claude Code (50) -> npx shim (55, node) -> atlas (60), with and
    // without a cmd /c between, and with start times unread.
    let shim = p(&[(9, 1, "sshd"), (50, 9, "node"), (55, 50, "node"), (60, 55, "atlas")]);
    assert_eq!(owner_pid(&shim, 60, at(&[(9, 1), (50, 10), (55, 20), (60, 30)])), Some(50));
    assert_eq!(owner_pid(&shim, 60, |_| None), Some(50));
    let cmd = p(&[(50, 4, "claude.exe"), (52, 50, "cmd.exe"), (55, 52, "node.exe"), (60, 55, "atlas.exe")]);
    assert_eq!(owner_pid(&cmd, 60, at(&[(50, 10), (52, 15), (55, 20), (60, 30)])), Some(50));
    // The wrapper (55) exited and an unrelated node.exe took its pid: it
    // started after the server, so it is not the owner.
    let reused = p(&[(50, 4, "explorer.exe"), (55, 50, "node.exe"), (60, 55, "atlas.exe")]);
    assert_eq!(owner_pid(&reused, 60, at(&[(50, 1), (55, 40), (60, 30)])), None);
    // Above a real owner, a reused pid named node is not climbed to.
    let above = p(&[(45, 4, "node.exe"), (50, 45, "claude.exe"), (60, 50, "atlas.exe")]);
    assert_eq!(owner_pid(&above, 60, at(&[(45, 50), (50, 10), (60, 30)])), Some(50));
    // A parent dated the same tick as its child is accepted.
    assert_eq!(owner_pid(&p(&[(50, 4, "claude"), (60, 50, "atlas")]), 60, at(&[(50, 30), (60, 30)])), Some(50));
}

/// #395 review: the next launch's slot is one past the highest number filed
/// within two stale windows, gaps and all: slots 1 and 5 filed inside the
/// window give 6 (not a count plus one), and a 9 filed just outside it is
/// not counted (a window of one or three stale periods gives 1 or 10).
#[test]
fn the_next_slot_follows_the_highest_in_the_window_past_gaps() {
    use std::time::{Duration, SystemTime};
    let t = Temp::new("orch-395-slot-window");
    let s = Session::new(t.path());
    let set = settings(t.path(), &builders(4), &[]);
    let window = set.orchestrate.stale * 2;
    let ten = Duration::from_secs(600);
    let now = SystemTime::now();
    let mut store = atlas::memory::MemoryStore::open(&set.store).unwrap();
    for (n, (slot, at)) in [(1, now - window + ten), (5, now - window + ten), (9, now - window - ten)].into_iter().enumerate() {
        let d = atlas::memory::Draft {
            title: format!("launch: old {n}"),
            meta: format!("agent: old{n}\ntype: general-purpose\nsession: s0\nslot: {slot}\n"),
            body: "Kind: builder\nLane: OLD\n\nOld.".into(),
            reason: "test".into(),
            author: "hook".into(),
            client: "atlas-hook".into(),
            time: Some(atlas::store::utc(at)),
            ..atlas::memory::Draft::default()
        };
        let mut w = store.writer().unwrap();
        w.create(&format!("journal:OLD:{:04}", n + 1), "journal", atlas::hooks::LAUNCH_ENTRY, "OLD", &d, None).unwrap();
        w.commit().unwrap();
    }
    drop(store);
    let mut h = Hooks::open(settings(t.path(), &builders(4), &[])).unwrap();
    h.enable_restart_point();
    let brief = "Kind: builder\nLane: WALRUS\n\nBuild the walrus parser.";
    assert!(launch_with(&mut h, &s, "t1", brief).deny.is_none());
    s.agent("b1", "general-purpose", "t1", brief, 1000, false);
    let _ = start(&mut h, &s, "b1");
    let entry = entries(&h, "WALRUS", "launch")[0].clone();
    assert!(entry.meta.contains("\nslot: 6\n"), "{}", entry.meta);
}
