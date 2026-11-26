//! A model of servers sharing one index: the per-root decisions of the
//! indexer's loop (`code_index::after_walk`, `stands_down`) driven through
//! random edits, edits during a run, marks, failed runs, and passes with and
//! without a full walk, on a simulated clock. The bookkeeping around them
//! mirrors `code_index::run`: a run stores its walk's fingerprint and the
//! server remembers that one; a watcher sees every edit; the lease serialises
//! runs; a server that finds the lease taken tries again after the quiet time.
//!
//! The properties, for one root:
//! - the servers settle: once events stop, no server runs again;
//! - no edit is lost: unless a run failed, the stored fingerprint is the last
//!   version's;
//! - no useless run: a server never starts a run whose fingerprint is stored.
use atlas::code_index::{after_walk, stands_down, Walked};
use proptest::prelude::*;

/// The quiet time, in passes.
const QUIET: i64 = 2;

#[derive(Debug, Clone)]
enum Event {
    Edit,
    Mark,
    /// A pass of one server's loop, with a full walk or without (a healthy
    /// watcher between safety-net walks).
    Pass(usize, bool),
    /// A server's run, if one is going, fails.
    Fail(usize),
}

#[derive(Debug, Default)]
struct Server {
    /// Which configuration or version: servers of one variant compute one
    /// fingerprint for the same files.
    variant: u8,
    seen: String,
    changed: Option<i64>,
    failed: Option<String>,
    started: bool,
    notified: bool,
    running: Option<String>,
    runs: u32,
}

#[derive(Debug, Default)]
struct World {
    version: u32,
    stored: Option<String>,
    lease: Option<usize>,
    now: i64,
    servers: Vec<Server>,
    useless: u32,
}

fn fingerprint(variant: u8, version: u32) -> String {
    format!("v{variant}:{version}")
}

impl World {
    fn new(variants: &[u8]) -> World {
        World { servers: variants.iter().map(|&variant| Server { variant, ..Server::default() }).collect(), ..World::default() }
    }

    fn edit(&mut self) {
        self.version += 1;
        for s in &mut self.servers {
            s.notified = true;
        }
    }

    fn pass(&mut self, i: usize, walk: bool) {
        self.now += 1;
        let (now, version) = (self.now, self.version);
        let s = &mut self.servers[i];
        if let Some(walked) = s.running.take() {
            // The run ends: it stores its walk's fingerprint, which the
            // server remembers.
            self.stored = Some(walked.clone());
            s.seen = walked;
            s.changed = None;
            s.failed = None;
            s.runs += 1;
            self.lease = None;
            return;
        }
        let current = fingerprint(s.variant, version);
        if walk || !s.started {
            let pending = s.changed.is_some();
            match after_walk(self.stored.as_deref(), &current, &s.seen, s.failed.as_deref(), pending, !s.started) {
                Walked::Current => {
                    s.changed = None;
                    // As the loop does: a walk that finds the stored fingerprint
                    // clears a failure.
                    if self.stored.as_deref() == Some(current.as_str()) {
                        s.failed = None;
                    }
                }
                Walked::Changing { at_once } => s.changed = Some(if at_once { now - QUIET } else { now }),
                Walked::Mark => s.changed = Some(now - QUIET),
                Walked::Keep => {}
            }
            s.seen = current.clone();
            s.started = true;
            s.notified = false;
        } else if s.notified {
            s.changed = Some(now);
            s.notified = false;
        }
        let Some(at) = s.changed else { return };
        if now - at < QUIET {
            return;
        }
        if self.lease.is_some_and(|h| h != i) {
            s.changed = Some(now);
            return;
        }
        if stands_down(self.stored.as_deref(), &current) {
            s.changed = None;
            s.seen = current;
            return;
        }
        if self.stored.as_deref() == Some(current.as_str()) {
            self.useless += 1;
        }
        self.lease = Some(i);
        s.changed = None;
        s.running = Some(current);
    }

    fn fail(&mut self, i: usize) {
        let s = &mut self.servers[i];
        if s.running.take().is_some() {
            s.failed = Some(s.seen.clone());
            s.changed = None;
            self.lease = None;
        }
    }

    /// Every server's passes in turn, a full walk every fourth round.
    fn quiet_rounds(&mut self, rounds: usize) {
        for r in 0..rounds {
            for i in 0..self.servers.len() {
                self.pass(i, r % 4 == 0);
            }
        }
    }

    fn runs(&self) -> Vec<u32> {
        self.servers.iter().map(|s| s.runs).collect()
    }
}

fn event(servers: usize) -> impl Strategy<Value = Event> {
    prop_oneof![
        2 => Just(Event::Edit),
        1 => Just(Event::Mark),
        8 => (0..servers, any::<bool>()).prop_map(|(i, w)| Event::Pass(i, w)),
        1 => (0..servers).prop_map(Event::Fail),
    ]
}

fn scenario() -> impl Strategy<Value = (Vec<u8>, Vec<Event>)> {
    prop::collection::vec(0u8..2, 1..5).prop_flat_map(|variants| {
        let n = variants.len();
        (Just(variants), prop::collection::vec(event(n), 0..60))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn servers_settle_index_every_edit_and_never_run_for_nothing((variants, events) in scenario()) {
        let mut w = World::new(&variants);
        let mut failed = false;
        for e in &events {
            match *e {
                Event::Edit => w.edit(),
                Event::Mark => w.stored = Some(String::new()),
                Event::Pass(i, walk) => w.pass(i, walk),
                Event::Fail(i) => {
                    failed |= w.servers[i].running.is_some();
                    w.fail(i);
                }
            }
        }
        w.quiet_rounds(40);
        let settled = w.runs();
        w.quiet_rounds(40);
        prop_assert_eq!(w.runs(), settled.clone(), "the servers did not settle: runs {:?}, variants {:?}", settled, variants);
        if !failed {
            let last = format!(":{}", w.version);
            prop_assert!(w.stored.as_deref().is_some_and(|f| f.ends_with(&last)), "the last edit was not indexed: stored {:?}, version {}", w.stored, w.version);
        }
        prop_assert_eq!(w.useless, 0, "a run started for a fingerprint already stored");
    }
}
