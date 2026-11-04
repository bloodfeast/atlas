//! Change notification for the configured roots, so a query checks only
//! the files that changed instead of walking every folder.
//!
//! Each root is watched by an owned FFI backend in [`crate::platform`] (no
//! crates): `ReadDirectoryChangesW` on Windows, inotify on Linux, FSEvents
//! on macOS. Every
//! reported path is marked dirty. A consumer
//! takes a [`Snapshot`] of a root's dirty set, brings those paths up to date,
//! and clears what it handled with [`Watcher::clear`], which keeps any path
//! reported after the snapshot.
//!
//! A root falls back to a full walk when:
//!
//! * the notification queue overflowed (`ERROR_NOTIFY_ENUM_DIR` or a
//!   zero-byte completion on Windows, `IN_Q_OVERFLOW` on Linux,
//!   `MustScanSubDirs` or a dropped-events flag on macOS): the root is marked
//!   `full`;
//! * the watch could not be opened or failed later: the root is marked
//!   unhealthy for good, and every check walks it, as on other platforms;
//! * the caller's safety-net interval passed (see
//!   [`crate::store::AtlasSetup::rescan_interval`]).
//!
//! On a platform without a backend [`Watcher::start`]
//! returns a watcher whose roots are all unhealthy, so the stat walk is used
//! everywhere, as it is under every backend as the periodic safety net.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// What a consumer sees of one root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// False when the watch is not running: the caller must walk.
    pub healthy: bool,
    /// True when events were lost (overflow): the caller must walk.
    pub full: bool,
    /// The dirty paths, `/`-separated, relative to the root, sorted.
    pub paths: Vec<String>,
    /// The sequence number to pass to [`Watcher::clear`].
    pub seq: u64,
}

#[derive(Debug, Default)]
struct RootState {
    healthy: bool,
    /// Sequence number of the overflow that requires a walk, if any.
    full: Option<u64>,
    dirty: HashMap<String, u64>,
}

#[derive(Debug, Default)]
struct State {
    roots: Vec<RootState>,
    seq: u64,
}

/// Where a backend reports what it saw.
pub trait Sink: Send + Sync {
    /// Marks paths (relative to the root, `/`-separated) dirty, or the
    /// whole root when events were lost.
    fn mark(&self, root: usize, paths: Vec<String>, overflow: bool);
    /// Marks the root's watch as failed for good.
    fn fail(&self, root: usize);
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

}

impl Sink for Shared {
    fn mark(&self, root: usize, paths: Vec<String>, overflow: bool) {
        let mut st = self.lock();
        st.seq += 1;
        let seq = st.seq;
        let r = &mut st.roots[root];
        if overflow {
            r.full = Some(seq);
        }
        for p in paths {
            r.dirty.insert(p, seq);
        }
        drop(st);
        self.changed.notify_all();
    }

    fn fail(&self, root: usize) {
        let mut st = self.lock();
        st.seq += 1;
        st.roots[root].healthy = false;
        drop(st);
        self.changed.notify_all();
    }
}

/// Pokes a [`Watcher`]'s roots from any thread (see [`Watcher::poke`]).
#[derive(Debug, Clone)]
pub struct Poker {
    shared: Arc<Shared>,
    roots: usize,
}

impl Poker {
    /// As [`Watcher::poke`].
    pub fn poke(&self, root: usize) {
        if root < self.roots {
            self.shared.mark(root, Vec::new(), true);
        }
    }

    /// The number of roots.
    pub fn roots(&self) -> usize {
        self.roots
    }
}

/// Watches a set of roots for changes.
#[derive(Debug)]
pub struct Watcher {
    shared: Arc<Shared>,
    roots: Vec<PathBuf>,
    backends: Vec<crate::platform::Backend>,
}

impl Watcher {
    /// Starts watching each root. A root that cannot be watched is marked
    /// unhealthy; nothing here fails. Each healthy root's watch is armed
    /// when this returns: a change made after it is reported.
    ///
    /// # Arguments
    ///
    /// * `roots` - folders to watch, each with its whole subtree.
    pub fn start(roots: &[PathBuf]) -> Watcher {
        let shared = Arc::new(Shared::default());
        shared.lock().roots = roots.iter().map(|_| RootState::default()).collect();
        let mut backends = Vec::new();
        for (i, r) in roots.iter().enumerate() {
            let sink: Arc<dyn Sink> = shared.clone();
            if let Some(b) = crate::platform::Backend::spawn(r, i, sink) {
                shared.lock().roots[i].healthy = true;
                backends.push(b);
            }
        }
        Watcher { shared, roots: roots.to_vec(), backends }
    }

    /// A watcher with no running watch, as on a platform without one.
    pub fn disabled(roots: &[PathBuf]) -> Watcher {
        let shared = Arc::new(Shared::default());
        shared.lock().roots = roots.iter().map(|_| RootState::default()).collect();
        Watcher { shared, roots: roots.to_vec(), backends: Vec::new() }
    }

    /// The index of a watched root, by path.
    pub fn root_index(&self, root: &Path) -> Option<usize> {
        self.roots.iter().position(|r| r == root)
    }

    /// The current state of one root.
    ///
    /// # Arguments
    ///
    /// * `root` - an index from [`Watcher::root_index`].
    pub fn snapshot(&self, root: usize) -> Snapshot {
        let st = self.shared.lock();
        let Some(r) = st.roots.get(root) else { return Snapshot::default() };
        let mut paths: Vec<String> = r.dirty.keys().cloned().collect();
        paths.sort_unstable();
        Snapshot { healthy: r.healthy, full: r.full.is_some(), paths, seq: st.seq }
    }

    /// True when the root is healthy with nothing dirty: a check of it can
    /// be skipped. Cheaper than [`Watcher::snapshot`]: nothing is copied.
    pub fn is_clean(&self, root: usize) -> bool {
        let st = self.shared.lock();
        st.roots.get(root).is_some_and(|r| r.healthy && r.full.is_none() && r.dirty.is_empty())
    }

    /// Forgets what a consumer handled: dirty paths and the overflow mark
    /// recorded at or before `seq`. Anything reported later stays.
    ///
    /// # Arguments
    ///
    /// * `root` - an index from [`Watcher::root_index`].
    /// * `seq` - the [`Snapshot::seq`] the consumer acted on.
    pub fn clear(&self, root: usize, seq: u64) {
        let mut st = self.shared.lock();
        if let Some(r) = st.roots.get_mut(root) {
            r.dirty.retain(|_, s| *s > seq);
            if r.full.is_some_and(|s| s <= seq) {
                r.full = None;
            }
        }
    }

    /// Asks for a root to be walked: marks it as if events were lost, and
    /// wakes anyone in [`Watcher::wait_past`].
    ///
    /// # Arguments
    ///
    /// * `root` - an index from [`Watcher::root_index`].
    pub fn poke(&self, root: usize) {
        if root < self.roots.len() {
            self.shared.mark(root, Vec::new(), true);
        }
    }

    /// A handle that can [`Poker::poke`] this watcher's roots from another
    /// thread (the watcher itself holds backend handles that stay on one).
    pub fn poker(&self) -> Poker {
        Poker { shared: self.shared.clone(), roots: self.roots.len() }
    }

    /// The count of events seen so far, over all roots.
    pub fn events(&self) -> u64 {
        self.shared.lock().seq
    }

    /// Blocks until more than `seen` events have been seen or `timeout`
    /// passes (a condition variable wait, not a poll), for tests and tools
    /// that must observe a change they just made.
    ///
    /// # Returns
    ///
    /// True when a new event arrived.
    pub fn wait_past(&self, seen: u64, timeout: Duration) -> bool {
        let st = self.shared.lock();
        let (st, _) = self.shared.changed.wait_timeout_while(st, timeout, |s| s.seq <= seen).unwrap_or_else(|p| p.into_inner());
        st.seq > seen
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        for b in &mut self.backends {
            b.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_keeps_later_events() {
        let w = Watcher::disabled(&[PathBuf::from("x")]);
        w.shared.mark(0, vec!["a.md".to_string()], false);
        let s = w.snapshot(0);
        assert_eq!(s.paths, vec!["a.md"]);
        w.shared.mark(0, vec!["b.md".to_string()], true);
        w.clear(0, s.seq);
        let s2 = w.snapshot(0);
        assert_eq!(s2.paths, vec!["b.md"]);
        assert!(s2.full);
        w.clear(0, s2.seq);
        assert!(!w.snapshot(0).full);
        assert!(!w.is_clean(0), "a disabled root is never clean");
    }

    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn sees_a_write_under_the_root() {
        let dir = std::env::temp_dir().join(format!("atlas-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let w = Watcher::start(&[dir.clone()]);
        assert!(w.snapshot(0).healthy);
        assert!(w.is_clean(0));
        let mut seen = w.events();
        std::fs::write(dir.join("sub").join("n.md"), "x").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !w.snapshot(0).paths.iter().any(|p| p == "sub/n.md") {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(w.wait_past(seen, left), "no event within 5 s");
            seen = w.events();
        }
        let s = w.snapshot(0);
        assert!(s.paths.iter().any(|p| p == "sub/n.md"), "{:?}", s.paths);
        w.clear(0, s.seq);
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
