//! The advance warning of a scheduled restart (#221, ruling
//! journal:ATLAS-RELEASE:0066 Q11 option 1).
//!
//! A poll thread in the resident server ([`start`]) reads, every
//! [`POLL_EVERY`] (macOS every [`MACOS_POLL_EVERY`]), whether the system has
//! a restart pending or scheduled:
//!
//! * Windows: the registry, read only by advapi32 FFI ([`WINDOWS_PROBES`]):
//!   Windows Update's `RebootRequired` key, the Update Orchestrator's
//!   `RebootRequired` state and Component Based Servicing's
//!   `RebootPending` key. None of them holds the time (the Update
//!   Orchestrator's reboot task file, which does, is administrators' only).
//! * Linux: `/run/systemd/shutdown/scheduled` (`shutdown -r +N`, with its
//!   time), `/run/reboot-required` (Debian and Ubuntu), and
//!   `needs-restarting -r` where it is installed (exit 1).
//! * macOS: `softwareupdate --list --no-scan`, an update whose action is a
//!   restart, from the system's last background scan (no network).
//!
//! On a change to a pending restart the restart point is written at once
//! and kept current while it lasts (`crate::platform::shutdown::hold`), and
//! the main session's next `UserPromptSubmit` gets one line ([`line`]),
//! once per change. Nothing here changes an update or power setting.

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the poll reads the platform's restart state.
pub const POLL_EVERY: Duration = Duration::from_secs(10 * 60);
/// How often on macOS, where `softwareupdate` is a process start.
pub const MACOS_POLL_EVERY: Duration = Duration::from_secs(60 * 60);
/// The first poll, this long after the server starts (off its start-up).
const FIRST_POLL: Duration = Duration::from_secs(60);

/// What one registry lookup found (see `crate::platform::registry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg {
    /// No such key or value (or it could not be read).
    Missing,
    /// The key exists (a lookup with no value name).
    Key,
    /// A `REG_DWORD` value.
    Dword(u32),
    /// A `REG_QWORD` value.
    Qword(u64),
    /// A value of another type.
    Other,
}

/// A pending or scheduled restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pending {
    /// `restart`, or `shutdown` for a scheduled power-off or halt.
    pub what: String,
    /// When it is scheduled (Unix seconds), where the source says.
    pub at: Option<u64>,
    /// What says so, one label each.
    pub sources: Vec<String>,
}

impl Pending {
    fn restart(source: impl Into<String>) -> Pending {
        Pending { what: "restart".into(), at: None, sources: vec![source.into()] }
    }
}

/// `/run/systemd/shutdown/scheduled` (systemd's `KEY=VALUE` lines: `USEC`,
/// the time in microseconds since the epoch, and `MODE`). `None` for a
/// text with neither.
pub fn systemd_scheduled(text: &str) -> Option<Pending> {
    let (mut usec, mut mode) = (None, None);
    for l in text.lines() {
        match l.trim().split_once('=') {
            Some(("USEC", v)) => usec = v.trim().parse::<u64>().ok(),
            Some(("MODE", v)) => mode = Some(v.trim().to_string()),
            _ => {}
        }
    }
    if usec.is_none() && mode.is_none() {
        return None;
    }
    let mode = mode.unwrap_or_else(|| "reboot".into());
    let what = if matches!(mode.as_str(), "poweroff" | "halt") { "shutdown" } else { "restart" };
    Some(Pending {
        what: what.into(),
        at: usec.filter(|u| *u > 0).map(|u| u / 1_000_000),
        sources: vec![format!("systemd scheduled {mode}")],
    })
}

/// `/run/reboot-required` (its text, when the file exists) and
/// `/run/reboot-required.pkgs` (the packages that asked, one a line).
pub fn reboot_required(text: Option<&str>, pkgs: Option<&str>) -> Option<Pending> {
    text?;
    let n = pkgs.map_or(0, |p| p.lines().filter(|l| !l.trim().is_empty()).count());
    Some(Pending::restart(match n {
        0 => "/run/reboot-required".to_string(),
        1 => "/run/reboot-required (1 package)".to_string(),
        n => format!("/run/reboot-required ({n} packages)"),
    }))
}

/// `needs-restarting -r`'s exit code (`None` when it is not installed or
/// did not run): 1 is a restart needed, 0 none, anything else unknown.
pub fn needs_restarting(code: Option<i32>) -> Option<Pending> {
    (code == Some(1)).then(|| Pending::restart("needs-restarting -r"))
}

/// The registry lookups (under `HKEY_LOCAL_MACHINE`): the key, the value
/// (`None`: the key's existence), and the label.
pub const WINDOWS_PROBES: [(&str, Option<&str>, &str); 3] = [
    (r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired", None, "Windows Update RebootRequired"),
    (r"SOFTWARE\Microsoft\WindowsUpdate\UX\StateVariables", Some("RebootRequired"), "Update Orchestrator RebootRequired"),
    (r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending", None, "Component Based Servicing RebootPending"),
];

/// The registry's restart state, each [`WINDOWS_PROBES`] lookup answered by
/// `read`: a key that exists, or a nonzero value, is a restart pending.
pub fn windows_registry(read: impl Fn(&str, Option<&str>) -> Reg) -> Option<Pending> {
    let sources: Vec<String> = WINDOWS_PROBES
        .iter()
        .filter(|(k, v, _)| match read(k, *v) {
            Reg::Key => v.is_none(),
            Reg::Dword(d) => d != 0,
            Reg::Qword(q) => q != 0,
            Reg::Missing | Reg::Other => false,
        })
        .map(|(_, _, l)| (*l).to_string())
        .collect();
    (!sources.is_empty()).then(|| Pending { what: "restart".into(), at: None, sources })
}

/// `softwareupdate --list --no-scan`'s text (stdout and stderr): the updates whose
/// action is a restart. Both forms: `* Label: X` with a `Title: ...,
/// Action: restart,` line (macOS 10.15 and later), and `* X` with a
/// `... [restart]` line (older).
pub fn softwareupdate(text: &str) -> Option<Pending> {
    let mut labels: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for l in text.lines() {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("* ") {
            current = Some(rest.strip_prefix("Label: ").unwrap_or(rest).trim().to_string());
            continue;
        }
        let low = t.to_ascii_lowercase();
        if low.contains("action: restart") || low.contains("[restart]") {
            if let Some(c) = current.take() {
                labels.push(c);
            }
        }
    }
    if labels.is_empty() {
        return None;
    }
    let mut source = format!("softwareupdate: {}", labels.iter().take(2).cloned().collect::<Vec<_>>().join(", "));
    if labels.len() > 2 {
        source.push_str(&format!(" and {} more", labels.len() - 2));
    }
    Some(Pending::restart(source))
}

/// One restart state from several sources: every source, the earliest
/// time, and `shutdown` only when every timed source says so.
pub fn merge(parts: impl IntoIterator<Item = Option<Pending>>) -> Option<Pending> {
    let parts: Vec<Pending> = parts.into_iter().flatten().collect();
    if parts.is_empty() {
        return None;
    }
    let mut sources: Vec<String> = parts.iter().flat_map(|p| p.sources.iter().cloned()).collect();
    sources.sort();
    sources.dedup();
    let at = parts.iter().filter_map(|p| p.at).min();
    let timed: Vec<&Pending> = parts.iter().filter(|p| p.at.is_some()).collect();
    let what = if !timed.is_empty() && timed.iter().all(|p| p.what == "shutdown") { "shutdown" } else { "restart" };
    Some(Pending { what: what.into(), at, sources })
}

/// What a poll saw, against the poll before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The same as last time.
    Same,
    /// A restart is pending now, and it was not, or not this one.
    Pending(Pending),
    /// No restart is pending any more.
    Cleared,
}

/// The last restart state the poll saw.
#[derive(Debug, Clone, Default)]
pub struct Watch {
    last: Option<Pending>,
}

impl Watch {
    /// Takes the state `now`; what changed.
    pub fn observe(&mut self, now: Option<Pending>) -> Change {
        if now == self.last {
            return Change::Same;
        }
        self.last.clone_from(&now);
        match now {
            Some(p) => Change::Pending(p),
            None => Change::Cleared,
        }
    }
}

/// The context line for the main session: the restart, its time where
/// known (UTC), how many agents run, and what says so. A restart point is
/// written only while agents run; with none the line says it will be.
pub fn line(p: &Pending, agents: usize) -> String {
    let when = p.at.map_or_else(
        || format!("{} pending (time not known)", p.what),
        |t| format!("{} scheduled at {}", p.what, crate::discussion::civil(i64::try_from(t).unwrap_or(i64::MAX))),
    );
    let run = match agents {
        0 => "while no agents run".to_string(),
        1 => "while 1 agent runs".to_string(),
        n => format!("while {n} agents run"),
    };
    let point = if agents == 0 { "a restart point will be written if agents start" } else { "a restart point is written and kept current" };
    format!(
        "atlas: {when} {run} ({}); {point}. Tell the user; pausing updates or moving active hours is their call, atlas changes no update setting.",
        p.sources.join(", ")
    )
}

/// The warning waiting for the main session's next prompt, shared by the
/// poll thread and the hook state.
#[derive(Debug, Clone, Default)]
pub struct Notices(Arc<Mutex<Option<Pending>>>);

impl Notices {
    /// Leaves `p` for the next prompt (replacing one not yet told).
    pub fn post(&self, p: Option<Pending>) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = p;
    }

    /// The warning not yet told, once.
    pub fn take(&self) -> Option<Pending> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()
    }
}

/// Acts on one poll's `change`: a pending restart writes the restart point
/// and keeps it current, and leaves the warning in `notices`; a cleared one
/// removes the point and the untold warning.
pub fn on_change(change: &Change, notices: &Notices) {
    match change {
        Change::Same => {}
        Change::Pending(p) => {
            crate::platform::shutdown::hold(true, &format!("{} pending ({})", p.what, p.sources.join(", ")));
            notices.post(Some(p.clone()));
        }
        Change::Cleared => {
            crate::platform::shutdown::hold(false, "restart no longer pending");
            notices.post(None);
        }
    }
}

/// How often this platform polls.
pub fn poll_every() -> Duration {
    if cfg!(target_os = "macos") {
        MACOS_POLL_EVERY
    } else {
        POLL_EVERY
    }
}

/// A command's run: its exit code and its stdout and stderr; `None` when it
/// could not start (not installed).
#[cfg(unix)]
fn run(cmd: &str, args: &[&str]) -> Option<(Option<i32>, String)> {
    let o = std::process::Command::new(cmd).args(args).stdin(std::process::Stdio::null()).output().ok()?;
    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&o.stderr));
    Some((o.status.code(), text))
}

/// This platform's restart state now.
pub fn poll() -> Option<Pending> {
    #[cfg(windows)]
    {
        windows_registry(crate::platform::registry::hklm)
    }
    #[cfg(target_os = "linux")]
    {
        let read = |p: &str| std::fs::read_to_string(p).ok();
        merge([
            read("/run/systemd/shutdown/scheduled").as_deref().and_then(systemd_scheduled),
            reboot_required(read("/run/reboot-required").as_deref(), read("/run/reboot-required.pkgs").as_deref()),
            needs_restarting(run("needs-restarting", &["-r"]).and_then(|r| r.0)),
        ])
    }
    #[cfg(target_os = "macos")]
    {
        // The last background scan's results: no network, no catalog fetch.
        run("softwareupdate", &["--list", "--no-scan"]).and_then(|r| softwareupdate(&r.1))
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Starts the poll thread, once per process; the warnings it leaves. A
/// thread that cannot start leaves none.
pub fn start() -> Notices {
    static STARTED: std::sync::OnceLock<Notices> = std::sync::OnceLock::new();
    STARTED
        .get_or_init(|| {
            let notices = Notices::default();
            let n = notices.clone();
            let spawned = std::thread::Builder::new().name("atlas-restart-warn".into()).spawn(move || {
                let mut watch = Watch::default();
                std::thread::sleep(FIRST_POLL);
                loop {
                    on_change(&watch.observe(poll()), &n);
                    std::thread::sleep(poll_every());
                }
            });
            if let Err(e) = spawned {
                eprintln!("atlas restart warning: {e}");
            }
            notices
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_scheduled_reads_its_time_and_mode() {
        let text = "USEC=1763343420000000\nWARN_WALL=1\nMODE=reboot\nWALL_MESSAGE=kernel update\n";
        let p = systemd_scheduled(text).unwrap();
        assert_eq!((p.what.as_str(), p.at), ("restart", Some(1_763_343_420)), "{p:?}");
        assert_eq!(p.sources, vec!["systemd scheduled reboot".to_string()]);
        let off = systemd_scheduled("USEC=1763343420000000\nMODE=poweroff\n").unwrap();
        assert_eq!(off.what, "shutdown");
        assert_eq!(systemd_scheduled(""), None);
        assert_eq!(systemd_scheduled("WARN_WALL=1\n"), None);
    }

    #[test]
    fn reboot_required_counts_its_packages() {
        assert_eq!(reboot_required(None, Some("linux-image\n")), None);
        let p = reboot_required(Some("*** System restart required ***\n"), Some("linux-image-6.8.0-49\nlibc6\n")).unwrap();
        assert_eq!(p.sources, vec!["/run/reboot-required (2 packages)".to_string()]);
        assert_eq!(p.at, None);
        assert_eq!(reboot_required(Some(""), None).unwrap().sources, vec!["/run/reboot-required".to_string()]);
    }

    #[test]
    fn needs_restarting_is_pending_only_on_exit_one() {
        assert!(needs_restarting(Some(1)).is_some());
        assert_eq!(needs_restarting(Some(0)), None);
        assert_eq!(needs_restarting(Some(2)), None);
        assert_eq!(needs_restarting(None), None);
    }

    #[test]
    fn the_registry_values_read_as_fixture_data() {
        use std::collections::HashMap;
        let fixture = |rows: &[(&str, Option<&str>, Reg)]| {
            let m: HashMap<(String, Option<String>), Reg> =
                rows.iter().map(|(k, v, r)| ((k.to_string(), v.map(str::to_string)), *r)).collect();
            move |k: &str, v: Option<&str>| m.get(&(k.to_string(), v.map(str::to_string))).copied().unwrap_or(Reg::Missing)
        };
        let (wu, ux, cbs) = (WINDOWS_PROBES[0].0, WINDOWS_PROBES[1].0, WINDOWS_PROBES[2].0);
        // This machine's own state on 2026-11-16: the UX value is 0, the
        // keys are absent.
        assert_eq!(windows_registry(fixture(&[(ux, Some("RebootRequired"), Reg::Dword(0))])), None);
        let p = windows_registry(fixture(&[(wu, None, Reg::Key), (ux, Some("RebootRequired"), Reg::Dword(1))])).unwrap();
        assert_eq!(p.sources, vec!["Windows Update RebootRequired".to_string(), "Update Orchestrator RebootRequired".to_string()]);
        assert_eq!(p.at, None);
        let cbs_only = windows_registry(fixture(&[(cbs, None, Reg::Key)])).unwrap();
        assert_eq!(cbs_only.sources, vec!["Component Based Servicing RebootPending".to_string()]);
        // A value of another type says nothing.
        assert_eq!(windows_registry(fixture(&[(ux, Some("RebootRequired"), Reg::Other)])), None);
    }

    /// The registry FFI on this machine: a key every Windows has, a DWORD
    /// value every Windows 10 and later has, and a key no Windows has. Read
    /// only; the restart state itself is not asserted (it is the machine's).
    #[cfg(windows)]
    #[test]
    fn the_registry_is_read_on_this_machine() {
        use crate::platform::registry::hklm;
        assert_eq!(hklm(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", None), Reg::Key);
        assert!(
            matches!(hklm(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", Some("CurrentMajorVersionNumber")), Reg::Dword(n) if n >= 10)
        );
        assert_eq!(hklm(r"SOFTWARE\atlas-no-such-key-221", None), Reg::Missing);
        assert_eq!(hklm(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", Some("atlas-no-such-value")), Reg::Missing);
        let _ = poll();
    }

    #[test]
    fn softwareupdate_finds_the_updates_that_restart() {
        let new = "Software Update Tool\n\nFinding available software\nSoftware Update found the following new or updated software:\n\
            * Label: Safari17.1VenturaAuto-17.1\n\tTitle: Safari, Version: 17.1, Size: 157066KiB, Recommended: YES, \n\
            * Label: macOS Ventura 13.6.1-22G313\n\tTitle: macOS Ventura 13.6.1, Version: 13.6.1, Size: 1036432KiB, Recommended: YES, Action: restart, \n";
        let p = softwareupdate(new).unwrap();
        assert_eq!(p.sources, vec!["softwareupdate: macOS Ventura 13.6.1-22G313".to_string()]);
        let old = "Software Update found the following new or updated software:\n   * macOS Catalina 10.15.7 Update-\n\
            \tmacOS Catalina 10.15.7 Update (10.15.7), 3271216K [recommended] [restart]\n";
        assert_eq!(softwareupdate(old).unwrap().sources, vec!["softwareupdate: macOS Catalina 10.15.7 Update-".to_string()]);
        assert_eq!(softwareupdate("Software Update Tool\n\nFinding available software\nNo new software available.\n"), None);
    }

    #[test]
    fn merged_sources_keep_the_earliest_time() {
        let m = merge([
            systemd_scheduled("USEC=2000000000000000\nMODE=reboot\n"),
            None,
            reboot_required(Some(""), None),
            systemd_scheduled("USEC=1900000000000000\nMODE=reboot\n"),
        ])
        .unwrap();
        assert_eq!(m.at, Some(1_900_000_000));
        assert_eq!(m.what, "restart");
        assert_eq!(m.sources.len(), 2, "{m:?}");
        assert_eq!(merge([None, None]), None);
    }

    /// A change only when the state changes. (What `on_change` does with
    /// one is tested with the catch's slot, in
    /// `restart::tests::the_catch_writes_the_running_agents_and_nothing_without_them`:
    /// the slot is process-wide.)
    #[test]
    fn a_change_only_when_the_state_changes() {
        let mut w = Watch::default();
        let a = systemd_scheduled("USEC=1763343420000000\nMODE=reboot\n");
        assert_eq!(w.observe(None), Change::Same, "nothing pending at the start is no change");
        assert_eq!(w.observe(a.clone()), Change::Pending(a.clone().unwrap()));
        assert_eq!(w.observe(a.clone()), Change::Same, "the same state again is no change");
        let later = systemd_scheduled("USEC=1763347020000000\nMODE=reboot\n");
        assert!(matches!(w.observe(later), Change::Pending(_)), "a new time is a change");
        assert_eq!(w.observe(None), Change::Cleared);
    }

    /// S3: the line says a restart point is written only while agents run;
    /// with none (a pending restart can last days) it is still told, and
    /// says one will be. Times are UTC.
    #[test]
    fn the_line_claims_a_point_only_while_agents_run() {
        let a = systemd_scheduled("USEC=1763343420000000\nMODE=reboot\n").unwrap();
        let two = line(&a, 2);
        assert!(two.contains("restart scheduled at 2025-11-17T01:37:00Z while 2 agents run"), "{two}");
        assert!(two.contains("a restart point is written and kept current"), "{two}");
        let pending = reboot_required(Some(""), None).unwrap();
        assert!(line(&pending, 1).contains("restart pending (time not known) while 1 agent runs"), "{}", line(&pending, 1));
        let none = line(&pending, 0);
        assert!(none.contains("while no agents run"), "{none}");
        assert!(!none.contains("is written"), "with no agents no point is written: {none}");
        assert!(none.contains("a restart point will be written if agents start"), "{none}");
    }
}
