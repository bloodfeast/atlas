//! Work queue lanes: what can run in parallel (`docs/design/queue-lanes.md`).
//!
//! Every brief is classified into a lane (a conflict domain), a priority
//! and the shared resources it uses, each field declared on a `Lane:`,
//! `Priority:` or `Uses:` line or inferred from the brief; a declaration
//! always wins. An item holds its lane while it runs unless its brief
//! declares `Uses: read-only` (Q-QL1). [`propose`] walks the waiting items in rank order (priority
//! class, raised by aging) and picks the batch that can start together:
//! a free slot of its kind, no writer already on its lane, a free unit of
//! each resource it uses.

use crate::orchestrate::Kind;

/// How many leading lines are read for declarations (as for `Kind:`).
const DECL_LINES: usize = 40;

/// A priority class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    /// Start before everything else.
    Urgent,
    /// The default.
    #[default]
    Normal,
    /// Start when nothing else waits, or once it has aged.
    Background,
}

impl Priority {
    /// Parses `urgent`, `normal` or `background` (any case).
    pub fn parse(s: &str) -> Option<Priority> {
        match s.trim().to_ascii_lowercase().as_str() {
            "urgent" => Some(Priority::Urgent),
            "normal" => Some(Priority::Normal),
            "background" => Some(Priority::Background),
            _ => None,
        }
    }

    /// Its name.
    pub fn name(self) -> &'static str {
        match self {
            Priority::Urgent => "urgent",
            Priority::Normal => "normal",
            Priority::Background => "background",
        }
    }

    fn class(self) -> i64 {
        self as i64
    }
}

/// Where a field's value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A declaration line in the brief.
    Declared,
    /// Inferred, with the words that matched.
    Inferred(String),
    /// Nothing matched: the default.
    Default,
}

impl Source {
    fn note(&self) -> String {
        match self {
            Source::Declared => "declared".into(),
            Source::Inferred(w) => format!("inferred: {w:?}"),
            Source::Default => "default".into(),
        }
    }
}

/// A brief's classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    /// Its lane, if any.
    pub lane: Option<String>,
    /// Where the lane came from.
    pub lane_src: Source,
    /// Its priority.
    pub priority: Priority,
    /// Where the priority came from.
    pub priority_src: Source,
    /// The resources it uses, sorted, each with where it came from.
    pub uses: Vec<(String, Source)>,
    /// Declared `Uses: read-only`: it shares its lane (Q-QL1).
    pub read_only: bool,
}

impl Class {
    /// The resource names.
    pub fn resources(&self) -> impl Iterator<Item = &str> {
        self.uses.iter().map(|(r, _)| r.as_str())
    }

    /// The meta lines a queued entry records (`atlas queue show`).
    pub fn meta(&self) -> String {
        let uses = if self.uses.is_empty() { "none".to_string() } else { self.uses.iter().map(|(r, s)| format!("{r} ({})", s.note())).collect::<Vec<_>>().join(", ") };
        format!(
            "lane_from: {}\npriority: {} ({})\nuses: {uses}{}",
            self.lane.as_ref().map_or_else(|| "none".to_string(), |_| self.lane_src.note()),
            self.priority.name(),
            self.priority_src.note(),
            if self.read_only { "\nread_only: declared" } else { "" }
        )
    }
}

/// The declarations among a brief's first lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Declared {
    /// `Lane:`.
    pub lane: Option<String>,
    /// `Priority:`.
    pub priority: Option<Priority>,
    /// `Uses:` (`none` is an empty list), without `read-only`.
    pub uses: Option<Vec<String>>,
    /// `Uses:` lists `read-only`.
    pub read_only: bool,
}

/// A declaration line's value: `Name: value`, with markdown bullets,
/// quotes, headings and bold around it allowed.
pub(crate) fn decl<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let l = line.trim().trim_start_matches(['-', '*', '>', '#', ' ', '\t']);
    let l = l.strip_prefix("**").unwrap_or(l);
    let head = l.get(..name.len())?;
    if !head.eq_ignore_ascii_case(name) {
        return None;
    }
    let rest = &l[name.len()..];
    let rest = rest.strip_prefix("**").unwrap_or(rest);
    let rest = rest.strip_prefix(':')?;
    let rest = rest.strip_prefix("**").unwrap_or(rest);
    Some(rest.trim().trim_matches(['`', '"', '\'', '*']).trim())
}

/// Reads the `Lane:`, `Priority:` and `Uses:` lines.
pub fn declared(brief: &str) -> Declared {
    let mut d = Declared::default();
    for line in brief.lines().take(DECL_LINES) {
        if d.lane.is_none() {
            if let Some(v) = decl(line, "lane") {
                let tok: String = v.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
                if !tok.is_empty() {
                    d.lane = Some(tok);
                }
            }
        }
        if d.priority.is_none() {
            if let Some(v) = decl(line, "priority") {
                d.priority = v.split(|c: char| !c.is_ascii_alphabetic()).next().and_then(Priority::parse);
            }
        }
        if d.uses.is_none() {
            if let Some(v) = decl(line, "uses") {
                let mut r: Vec<String> = v
                    .split([',', ' '])
                    .map(|x| x.trim().trim_matches(['`', '"', '\'', '.']).to_string())
                    .filter(|x| !x.is_empty() && !x.eq_ignore_ascii_case("none") && !x.eq_ignore_ascii_case("and"))
                    .map(|x| x.to_ascii_lowercase())
                    .collect();
                r.sort();
                r.dedup();
                let before = r.len();
                r.retain(|x| x != "read-only" && x != "readonly");
                d.read_only = r.len() < before;
                d.uses = Some(r);
            }
        }
    }
    d
}

/// Whether the words before a match on its line negate it (the last 32
/// bytes, already lowercase).
fn negated(line_before: &[u8]) -> bool {
    let tail = &line_before[line_before.len().saturating_sub(32)..];
    ["no ", "not ", "never ", "don't ", "do not ", "without ", "nor ", "avoid "].iter().any(|w| tail.windows(w.len()).any(|x| x == w.as_bytes()))
}

const URGENT: &[&str] = &["urgent", "urgently", "blocker", "hotfix", "asap", "right away"];
const BACKGROUND: &[&str] = &["low priority", "low-priority", "no rush", "when idle", "when you have time", "nice to have"];
const GPU: &[&str] = &["gpu lock", "gpu-lock", "capture", "captures", "screenshot", "screenshots", "gpu timing", "gpu timings", "gpu bench", "benchmark", "benchmarks", "bake", "bakes", "render it", "quiet gpu", "gpu runs", "gpu run", "gpu time", "bench"];
const COMPILE: &[&str] = &["cargo build", "cargo test", "cargo nextest", "cargo xtask", "cargo clippy", "heavy compile", "release build", "cargo_build_jobs", "cargo_target_dir", "build jobs"];
const OWN_WORKTREE: &[&str] = &["your own worktree", "make a worktree", "create a worktree", "own worktree", "new worktree"];

/// The phrase groups [`scan`] looks for, in this order.
const GROUPS: [&[&str]; 4] = [URGENT, BACKGROUND, GPU, COMPILE];

/// How many phrases there are in all.
const PHRASES: usize = URGENT.len() + BACKGROUND.len() + GPU.len() + COMPILE.len();

/// Every phrase with its group, in group order.
const FLAT: [(usize, &str); PHRASES] = {
    let mut out = [(0, ""); PHRASES];
    let (mut g, mut k) = (0, 0);
    while g < GROUPS.len() {
        let mut j = 0;
        while j < GROUPS[g].len() {
            out[k] = (g, GROUPS[g][j]);
            k += 1;
            j += 1;
        }
        g += 1;
    }
    out
};

/// By first byte, the phrases (bits of [`FLAT`]) that start with it.
const FIRST: [u64; 256] = {
    assert!(PHRASES <= 64);
    let mut t = [0u64; 256];
    let mut k = 0;
    while k < PHRASES {
        t[FLAT[k].1.as_bytes()[0] as usize] |= 1 << k;
        k += 1;
    }
    t
};

/// For each of [`GROUPS`], the first phrase found as whole words in the
/// lowercased brief and not negated on its line: one pass over the word
/// starts, checking only the phrases that start with the word's first
/// byte, with no allocation.
fn scan(lower: &str) -> [Option<&'static str>; 4] {
    let b = lower.as_bytes();
    let mut hit: [Option<&'static str>; 4] = [None; 4];
    let mut found = 0u32;
    let mut line = 0;
    let mut in_word = false;
    for (i, &c) in b.iter().enumerate() {
        let was = std::mem::replace(&mut in_word, c.is_ascii_alphanumeric());
        if c == b'\n' {
            line = i + 1;
            continue;
        }
        if was {
            continue;
        }
        let mut m = FIRST[c as usize];
        while m != 0 {
            let k = m.trailing_zeros() as usize;
            m &= m - 1;
            let (g, p) = FLAT[k];
            let pb = p.as_bytes();
            // The second byte rules out most candidates before the compare.
            if found & (1 << g) == 0 && b.get(i + 1) == pb.get(1) && b[i..].starts_with(pb) && b.get(i + p.len()).is_none_or(|n| !n.is_ascii_alphanumeric()) && !negated(&b[line..i]) {
                hit[g] = Some(p);
                found |= 1 << g;
            }
        }
        if found == (1 << GROUPS.len()) - 1 {
            break;
        }
    }
    hit
}

/// The worktrees a brief points at by path: the folder after a path
/// segment named `worktrees` (`.../worktrees/NAME`). `lower` is the brief
/// lowercased (same byte offsets).
fn worktree_paths(brief: &str, lower: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, m) in lower.match_indices("worktrees") {
        let before_ok = i == 0 || matches!(lower.as_bytes()[i - 1], b'/' | b'\\' | b'-' | b'_' | b'.');
        let rest = &brief[i + m.len()..];
        let Some(rest) = rest.strip_prefix(['/', '\\']) else { continue };
        let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
        let name = name.trim_end_matches('.');
        if before_ok && !name.is_empty() && name != "<name>" && !name.contains('.') {
            let r = format!("worktree:{name}");
            if !out.contains(&r) {
                out.push(r);
            }
        }
    }
    out
}

/// An id-like token (two or more characters, uppercase letters, digits,
/// `-` or `_`, at least one letter, no lowercase).
fn id_like(t: &str) -> bool {
    t.len() >= 2 && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-' || c == '_') && t.chars().any(|c| c.is_ascii_uppercase())
}

/// The work a brief names when it names no lane: `package ID`, or
/// `the ID lane`, whichever comes first. (`package ID` is one workspace's
/// habit; it stays while inference does, since the `lanes-eval` gate
/// holds 8 lanes on it.) Only the words around each `package` and `lane`
/// in the lowercased brief are read.
fn named_work(brief: &str, lower: &str) -> Option<String> {
    fn trim(w: &str) -> &str {
        w.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    }
    let ws = char::is_whitespace;
    // The whitespace-separated word around byte `i`: its start and end.
    let around = |i: usize| {
        let s = brief[..i].char_indices().rev().find(|(_, c)| ws(*c)).map_or(0, |(j, c)| j + c.len_utf8());
        (s, brief[i..].find(ws).map_or(brief.len(), |j| i + j))
    };
    let mut best: Option<(usize, &str)> = None;
    for (i, _) in lower.match_indices("package") {
        let (s, e) = around(i);
        let next = trim(brief[e..].split_whitespace().next().unwrap_or(""));
        if trim(&brief[s..e]).eq_ignore_ascii_case("package") && id_like(next) {
            best = Some((s, next));
            break;
        }
    }
    for (i, _) in lower.match_indices("lane") {
        let (s, e) = around(i);
        if best.is_some_and(|(p, _)| p <= s) {
            break;
        }
        let prev = trim(brief[..s].split_whitespace().next_back().unwrap_or(""));
        if trim(&brief[s..e]).eq_ignore_ascii_case("lane") && id_like(prev) && prev != "LANE" {
            best = Some((s, prev));
            break;
        }
    }
    best.map(|(_, w)| w.to_string())
}

/// The resources a brief's words say it uses, from [`scan`]'s hits.
fn infer_uses(brief: &str, lower: &str, hit: &[Option<&'static str>; 4]) -> Vec<(String, Source)> {
    let mut out: Vec<(String, Source)> = Vec::new();
    if let Some(w) = hit[2] {
        out.push(("gpu".into(), Source::Inferred(w.into())));
    }
    if let Some(w) = hit[3] {
        out.push(("compile".into(), Source::Inferred(w.into())));
    }
    if !OWN_WORKTREE.iter().any(|p| lower.contains(p)) {
        for r in worktree_paths(brief, lower) {
            out.push((r, Source::Inferred("a worktree path".into())));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A brief's priority, from its words.
pub fn infer_priority(brief: &str) -> (Priority, Source) {
    priority_of(&scan(&brief.to_ascii_lowercase()))
}

fn priority_of(hit: &[Option<&'static str>; 4]) -> (Priority, Source) {
    match (hit[0], hit[1]) {
        (Some(w), _) => (Priority::Urgent, Source::Inferred(w.into())),
        (None, Some(w)) => (Priority::Background, Source::Inferred(w.into())),
        (None, None) => (Priority::Normal, Source::Default),
    }
}

/// Classifies a brief: each field declared, else inferred, else the
/// default. `known` is the lanes the store holds (for lane inference).
pub fn classify(brief: &str, known: &[String]) -> Class {
    let d = declared(brief);
    // One lowercase copy and one scan serve every inference, made only
    // when a field is not declared.
    let lower = if d.lane.is_none() || d.priority.is_none() || d.uses.is_none() { brief.to_ascii_lowercase() } else { String::new() };
    let (lane, lane_src) = match d.lane {
        Some(l) => (Some(l), Source::Declared),
        None => match crate::hooks::brief_lane_lower(brief, &lower, known).or_else(|| named_work(brief, &lower)) {
            Some(l) => (Some(l), Source::Inferred("the lane the brief names".into())),
            None => (None, Source::Default),
        },
    };
    let hit = if d.priority.is_none() || d.uses.is_none() { scan(&lower) } else { [None; 4] };
    let (priority, priority_src) = match d.priority {
        Some(p) => (p, Source::Declared),
        None => priority_of(&hit),
    };
    let uses = match d.uses {
        Some(u) => u.into_iter().map(|r| (r, Source::Declared)).collect(),
        None => infer_uses(brief, &lower, &hit),
    };
    Class { lane, lane_src, priority, priority_src, uses, read_only: d.read_only }
}

/// Resource capacities and the aging step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Capacity by resource (or class, the part before `:`); others have 1;
    /// 0 never serializes.
    pub resources: Vec<(String, usize)>,
    /// Minutes of waiting that raise an item one class (0: no aging).
    pub aging_minutes: i64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { resources: vec![("gpu".into(), 1), ("compile".into(), 1)], aging_minutes: 60 }
    }
}

impl Policy {
    /// A resource's capacity: its own entry, else its class's, else 1.
    pub fn capacity(&self, r: &str) -> usize {
        let class = r.split(':').next().unwrap_or(r);
        self.resources.iter().find(|(n, _)| n == r).or_else(|| self.resources.iter().find(|(n, _)| n == class)).map_or(1, |(_, c)| *c)
    }

    /// An item's rank: lower starts first.
    pub fn rank(&self, p: Priority, waited_minutes: i64) -> i64 {
        if self.aging_minutes <= 0 {
            return p.class() * 1_000_000 - waited_minutes.min(999_999);
        }
        p.class() * self.aging_minutes - waited_minutes
    }
}

/// The lane an item of `kind` holds while it runs: its lane, unless it
/// declared `Uses: read-only` (Q-QL1) or is a review. A review waits only
/// for a reviewer slot, never for a lane held by a builder or another
/// review, and holds none (journal ATLAS-V03:0095): two reviews of one PR
/// run together, beside the builder that owns the lane.
pub fn held_lane(kind: Kind, class: &Class) -> Option<&String> {
    class.lane.as_ref().filter(|_| !class.read_only && kind != Kind::Reviewer)
}

/// Lanes compare without case (`Lane: parser` and `PARSER` are one lane).
fn same_lane(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// A waiting item.
#[derive(Debug, Clone)]
pub struct Item {
    /// Its queue key.
    pub key: String,
    /// Its kind.
    pub kind: Kind,
    /// Its classification.
    pub class: Class,
    /// Minutes since it was queued.
    pub waited_minutes: i64,
    /// Its place in a planned order (lower first, after priority).
    pub order: Option<Order>,
    /// It takes an agent slot (not an item the orchestrator does itself).
    pub takes_slot: bool,
    /// The `after` conditions not yet met; it waits while any is.
    pub unmet: Vec<String>,
}

/// A place in a planned order: a decimal number (`9`, `9.5`, `-0.25`) of any
/// length, compared digit by digit and printed exactly as written.
///
/// Not an `f64`: the numbers are written by hand, read back and compared, so
/// the type carries the digits rather than a nearest binary value, there is no
/// NaN to exclude from the order, and two written values never round to one
/// another. Not fixed point either: a limit on the digits is a value that
/// would read as no order at all. Spellings of one number (`9.5`, `9.50`) are
/// equal, and each still prints as written.
#[derive(Debug, Clone)]
pub struct Order(String);

impl Order {
    /// Negative (never for a zero), the whole digits without leading zeros,
    /// the fraction's digits without trailing zeros.
    fn parts(&self) -> (bool, &str, &str) {
        let digits = self.0.trim_start_matches(['+', '-']);
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        let (whole, fraction) = (whole.trim_start_matches('0'), fraction.trim_end_matches('0'));
        (self.0.starts_with('-') && !(whole.is_empty() && fraction.is_empty()), whole, fraction)
    }

    /// The sort key of a possibly missing order: lower first, none last.
    #[must_use]
    pub fn key(order: Option<&Self>) -> (bool, Option<&Self>) {
        (order.is_none(), order)
    }
}

impl std::str::FromStr for Order {
    type Err = ();

    /// An optional sign, digits, and optionally a point and more digits.
    fn from_str(s: &str) -> std::result::Result<Self, ()> {
        let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, "0"));
        let all_digits = |d: &str| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit());
        if all_digits(whole) && all_digits(fraction) {
            Ok(Self(s.to_string()))
        } else {
            Err(())
        }
    }
}

impl std::fmt::Display for Order {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Ord for Order {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let ((a_neg, a_whole, a_fraction), (b_neg, b_whole, b_fraction)) = (self.parts(), other.parts());
        // Without leading zeros the longer whole part is the larger, and
        // without trailing zeros the fractions compare as text.
        let size = a_whole.len().cmp(&b_whole.len()).then_with(|| a_whole.cmp(b_whole)).then_with(|| a_fraction.cmp(b_fraction));
        match (a_neg, b_neg) {
            (false, false) => size,
            (true, true) => size.reverse(),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
        }
    }
}

impl PartialOrd for Order {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Order {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Order {}

/// Something running that holds slots, lanes and resources.
#[derive(Debug, Clone)]
pub struct Holder {
    /// How it is named in a reason (an agent id or description).
    pub name: String,
    /// Its kind.
    pub kind: Kind,
    /// Its classification.
    pub class: Class,
}

/// The proposal: which items start together, and why the others wait.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proposal {
    /// Indices into the items, in rank order.
    pub start: Vec<usize>,
    /// The others, in rank order, each with its reason.
    pub wait: Vec<(usize, String)>,
}

/// The class with only the declared fields: inferred lanes and resources
/// never schedule (they did not meet the Q-QL3 bar on `lanes-eval`).
fn scheduled(class: &Class) -> Class {
    let mut c = class.clone();
    if c.lane_src != Source::Declared {
        c.lane = None;
    }
    c.uses.retain(|(_, s)| *s == Source::Declared);
    c
}

/// The batch that can start now. `free_slots(kind)` is how many more of a
/// kind may start; `total_free` bounds the whole batch.
///
/// The highest-ranked item that waits on a lane or a resource reserves
/// them: no lower-ranked item takes that lane or a unit of those
/// resources, so an item that needs two resources is not starved by items
/// that need one, and aging, which raises its rank, does reach it.
pub fn propose(items: &[Item], running: &[Holder], free_slots: &dyn Fn(Kind) -> usize, total_free: usize, policy: &Policy) -> Proposal {
    let items: Vec<Item> = items.iter().map(|it| Item { class: scheduled(&it.class), ..it.clone() }).collect();
    let running: Vec<Holder> = running.iter().map(|h| Holder { class: scheduled(&h.class), ..h.clone() }).collect();
    let (items, running) = (&items[..], &running[..]);
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| (policy.rank(items[i].class.priority, items[i].waited_minutes), Order::key(items[i].order.as_ref()), -items[i].waited_minutes, i));
    // Lanes held: lane -> who holds it.
    let mut lanes: Vec<(String, String)> = running.iter().filter_map(|h| held_lane(h.kind, &h.class).map(|l| (l.clone(), h.name.clone()))).collect();
    // Resource holders: (resource, who).
    let mut held: Vec<(String, String)> = running.iter().flat_map(|h| h.class.resources().map(|r| (r.to_string(), h.name.clone())).collect::<Vec<_>>()).collect();
    let mut taken: Vec<(Kind, usize)> = Vec::new();
    let mut total = 0usize;
    let mut reserved = false;
    let mut p = Proposal::default();
    for i in order {
        let it = &items[i];
        let used = taken.iter().find(|(k, _)| *k == it.kind).map_or(0, |(_, n)| *n);
        let reason = if !it.unmet.is_empty() {
            Some(format!("after {}", it.unmet.join("; ")))
        } else if it.takes_slot && total >= total_free {
            Some("no agent slot is free".to_string())
        } else if it.takes_slot && used >= free_slots(it.kind) {
            Some(format!("{} slots full", it.kind.name()))
        } else {
            let lane = held_lane(it.kind, &it.class).and_then(|l| lanes.iter().find(|(x, _)| same_lane(x, l))).map(|(l, who)| format!("lane {l}, after {who}"));
            let res = || {
                it.class.resources().find_map(|r| {
                    let cap = policy.capacity(r);
                    let holders: Vec<&String> = held.iter().filter(|(x, _)| x == r).map(|(_, w)| w).collect();
                    (cap > 0 && holders.len() >= cap).then(|| format!("{r} held by {}", holders.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")))
                })
            };
            // Read only when it decides: the reason when no lane blocks, or
            // what the head of line reserves.
            let res = if lane.is_none() || !reserved { res() } else { None };
            if (lane.is_some() || res.is_some()) && !reserved {
                // Head of line: hold its lane, and its resources only when a
                // resource is what blocks it (an item waiting on a lane must
                // not idle the gpu meanwhile).
                reserved = true;
                let who = format!("{} (reserved)", it.key);
                if let Some(l) = held_lane(it.kind, &it.class) {
                    lanes.push((l.clone(), who.clone()));
                }
                if res.is_some() {
                    for x in it.class.resources() {
                        held.push((x.to_string(), who.clone()));
                    }
                }
            }
            lane.or(res)
        };
        match reason {
            Some(r) => p.wait.push((i, r)),
            None => {
                p.start.push(i);
                if it.takes_slot {
                    total += 1;
                    match taken.iter_mut().find(|(k, _)| *k == it.kind) {
                        Some(t) => t.1 += 1,
                        None => taken.push((it.kind, 1)),
                    }
                }
                if let Some(l) = held_lane(it.kind, &it.class) {
                    lanes.push((l.clone(), it.key.clone()));
                }
                for r in it.class.resources() {
                    held.push((r.to_string(), it.key.clone()));
                }
            }
        }
    }
    p
}

/// One conflict of a launch with what runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// What conflicts (`lane parser held by a1`, `gpu held by a2`).
    pub what: String,
    /// The launch declared the field and declared holders alone fill it
    /// (Q-QL2: deny and queue); otherwise an inference is involved (warn).
    pub declared: bool,
}

/// The conflicts a launch would have with what runs. Empty when none.
pub fn conflicts(kind: Kind, class: &Class, running: &[Holder], policy: &Policy) -> Vec<Conflict> {
    let names = |hs: &[&Holder]| hs.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", ");
    let mut out = Vec::new();
    if let Some(l) = held_lane(kind, class) {
        let hs: Vec<&Holder> = running.iter().filter(|h| held_lane(h.kind, &h.class).is_some_and(|x| same_lane(x, l))).collect();
        if !hs.is_empty() {
            let decl: Vec<&Holder> = hs.iter().copied().filter(|h| h.class.lane_src == Source::Declared).collect();
            let declared = class.lane_src == Source::Declared && !decl.is_empty();
            out.push(Conflict { what: format!("lane {l} held by {}", names(if declared { &decl } else { &hs })), declared });
        }
    }
    for (r, src) in &class.uses {
        let cap = policy.capacity(r);
        let hs: Vec<(&Holder, bool)> = running.iter().filter_map(|h| h.class.uses.iter().find(|(x, _)| x == r).map(|(_, s)| (h, *s == Source::Declared))).collect();
        if cap > 0 && hs.len() >= cap {
            let decl: Vec<&Holder> = hs.iter().filter(|(_, d)| *d).map(|(h, _)| *h).collect();
            let all: Vec<&Holder> = hs.iter().map(|(h, _)| *h).collect();
            let declared = *src == Source::Declared && decl.len() >= cap;
            out.push(Conflict { what: format!("{r} held by {}", names(if declared { &decl } else { &all })), declared });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, kind: Kind, brief: &str, waited: i64) -> Item {
        Item { key: key.into(), kind, class: classify(brief, &[]), waited_minutes: waited, order: None, takes_slot: true, unmet: Vec::new() }
    }

    #[test]
    fn declarations_are_read_and_win() {
        let b = "- **Kind:** builder\n- Lane: parser\n**Priority:** urgent\nUses: `gpu`, compile, worktree:fix-a\n\nRun the captures. This is low priority.";
        let c = classify(b, &[]);
        assert_eq!(c.lane.as_deref(), Some("parser"));
        assert_eq!(c.lane_src, Source::Declared);
        assert_eq!((c.priority, &c.priority_src), (Priority::Urgent, &Source::Declared));
        assert_eq!(c.resources().collect::<Vec<_>>(), ["compile", "gpu", "worktree:fix-a"]);
        assert!(c.uses.iter().all(|(_, s)| *s == Source::Declared));
        // `Uses: none` turns inference off.
        assert!(classify("Uses: none\nRun cargo test and a capture.", &[]).uses.is_empty());
    }

    #[test]
    fn missing_fields_are_inferred() {
        let c = classify("Journal to lane PARSER-FIX.\nFix the reader, then run cargo test -j 16 and take a capture of the menu.\nWork in H:/w/repo-worktrees/parser-fix.", &[]);
        assert_eq!(c.lane.as_deref(), Some("PARSER-FIX"));
        assert_eq!(c.resources().collect::<Vec<_>>(), ["compile", "gpu", "worktree:parser-fix"]);
        assert_eq!(c.priority, Priority::Normal);
        assert_eq!(c.priority_src, Source::Default);
        // A negated phrase and a worktree of its own are not uses.
        let c = classify("Do not run cargo test; never take a capture. Make your own worktree in /w/worktrees/x.", &[]);
        assert!(c.uses.is_empty(), "{:?}", c.uses);
        assert_eq!(infer_priority("This is a blocker for the release.").0, Priority::Urgent);
        assert_eq!(infer_priority("No rush on this one.").0, Priority::Background);
    }

    #[test]
    fn the_batch_respects_slots_lanes_and_resources() {
        let items = vec![
            item("q1", Kind::Builder, "Lane: a\nUses: compile", 10),
            item("q2", Kind::Builder, "Lane: a\nUses: none", 50),
            item("q3", Kind::Builder, "Lane: b\nUses: compile", 40),
            item("q4", Kind::Reviewer, "Lane: a\nUses: read-only", 5),
            item("q6", Kind::Reviewer, "Lane: a\nUses: none", 4),
            item("q5", Kind::Builder, "Lane: c\nUses: gpu", 1),
        ];
        let running = vec![Holder { name: "r1".into(), kind: Kind::Builder, class: classify("Lane: z\nUses: gpu", &[]) }];
        let p = propose(&items, &running, &|k| if k == Kind::Builder { 3 } else { 2 }, 9, &Policy::default());
        let keys = |v: &[usize]| v.iter().map(|&i| items[i].key.clone()).collect::<Vec<_>>();
        // Oldest normal first: q2 takes lane a, q3 takes compile; the
        // reviews q4 and q6 hold no lane, so both start beside q2.
        assert_eq!(keys(&p.start), ["q2", "q3", "q4", "q6"]);
        let why: Vec<(String, String)> = p.wait.iter().map(|(i, r)| (items[*i].key.clone(), r.clone())).collect();
        assert_eq!(why, [("q1".to_string(), "lane a, after q2".to_string()), ("q5".to_string(), "gpu held by r1".to_string())]);
    }

    #[test]
    fn urgent_goes_first_and_aging_lifts_background() {
        let items = vec![item("bg", Kind::Builder, "Priority: background\nUses: compile", 130), item("ur", Kind::Builder, "Priority: urgent\nUses: compile", 0), item("no", Kind::Builder, "Uses: compile", 30)];
        let pol = Policy::default();
        let p = propose(&items, &[], &|_| 5, 5, &pol);
        // bg has waited 130 min: rank 120 - 130 = -10 beats urgent's 0.
        assert_eq!(items[p.start[0]].key, "bg");
        let young = vec![item("bg", Kind::Builder, "Priority: background\nUses: compile", 10), items[1].clone(), items[2].clone()];
        let p = propose(&young, &[], &|_| 5, 5, &pol);
        assert_eq!(young[p.start[0]].key, "ur");
        assert_eq!(p.wait.iter().map(|(i, _)| young[*i].key.as_str()).collect::<Vec<_>>(), ["no", "bg"]);
    }

    #[test]
    fn inferred_fields_never_schedule() {
        let items = vec![item("a", Kind::Builder, "Journal to lane A-ONE. Take a capture.", 0)];
        let running = vec![Holder { name: "r".into(), kind: Kind::Builder, class: classify("Lane: A-ONE\nUses: gpu", &[]) }];
        assert_eq!(propose(&items, &running, &|_| 5, 5, &Policy::default()).start, [0]);
    }

    #[test]
    fn the_top_waiter_reserves_what_it_needs() {
        // `big` ranks first and waits on the gpu; without the reservation
        // `small` would take the one compile unit, and the next, forever.
        let items = vec![item("big", Kind::Builder, "Uses: gpu, compile", 100), item("small", Kind::Builder, "Uses: compile", 10), item("free", Kind::Builder, "Uses: none", 5)];
        let running = vec![Holder { name: "r1".into(), kind: Kind::Builder, class: classify("Uses: gpu", &[]) }];
        let p = propose(&items, &running, &|_| 5, 5, &Policy::default());
        assert_eq!(p.start, [2]);
        let why: Vec<(&str, &str)> = p.wait.iter().map(|(i, r)| (items[*i].key.as_str(), r.as_str())).collect();
        assert_eq!(why, [("big", "gpu held by r1"), ("small", "compile held by big (reserved)")]);
        // A lane is reserved the same way.
        let items = vec![item("w1", Kind::Builder, "Lane: a\nUses: gpu", 100), item("w2", Kind::Builder, "Lane: a\nUses: none", 10)];
        let p = propose(&items, &running, &|_| 5, 5, &Policy::default());
        assert!(p.start.is_empty());
        assert_eq!(p.wait[1].1, "lane a, after w1 (reserved)");
        // Blocked only on its lane, the top waiter does not hold the gpu.
        let running = vec![Holder { name: "r2".into(), kind: Kind::Builder, class: classify("Lane: a\nUses: none", &[]) }];
        let items = vec![item("w1", Kind::Builder, "Lane: a\nUses: gpu", 100), item("g", Kind::Builder, "Lane: b\nUses: gpu", 10), item("w3", Kind::Builder, "Lane: a\nUses: none", 5)];
        let p = propose(&items, &running, &|_| 5, 5, &Policy::default());
        assert_eq!(p.start, [1]);
        let why: Vec<(&str, &str)> = p.wait.iter().map(|(i, r)| (items[*i].key.as_str(), r.as_str())).collect();
        assert_eq!(why, [("w1", "lane a, after r2"), ("w3", "lane a, after r2")]);
    }

    #[test]
    fn declared_holders_at_capacity_deny_beside_an_inferred_one() {
        let pol = Policy { resources: vec![("compile".into(), 2)], aging_minutes: 60 };
        let h = |n: &str, b: &str| Holder { name: n.into(), kind: Kind::Builder, class: classify(b, &[]) };
        let running = vec![h("r1", "Lane: parser\nUses: compile"), h("r2", "Uses: compile"), h("r3", "Journal to lane PARSER. Run cargo test.")];
        let c = conflicts(Kind::Builder, &classify("Lane: PARSER\nUses: compile", &[]), &running, &pol);
        let what: Vec<(&str, bool)> = c.iter().map(|c| (c.what.as_str(), c.declared)).collect();
        // Lanes compare without case; r3's lane and compile are inferred.
        assert_eq!(what, [("lane PARSER held by r1", true), ("compile held by r1, r2", true)]);
        // One declared holder of two units: only a warning.
        let c = conflicts(Kind::Builder, &classify("Lane: other\nUses: compile", &[]), &running[1..], &pol);
        assert_eq!(c.iter().map(|c| (c.what.as_str(), c.declared)).collect::<Vec<_>>(), [("compile held by r2, r3", false)]);
    }

    /// Journal ATLAS-V03:0095: a review waits only for a reviewer slot. It
    /// neither conflicts on nor holds a lane, so two reviews of one PR run
    /// together beside the builder in that lane, within the reviewer cap.
    #[test]
    fn reviews_wait_only_for_a_reviewer_slot() {
        let pol = Policy::default();
        let builder = Holder { name: "b1".into(), kind: Kind::Builder, class: classify("Lane: ATLAS-V03", &[]) };
        let review = Holder { name: "v1".into(), kind: Kind::Reviewer, class: classify("Lane: ATLAS-V03", &[]) };
        let simplicity = classify("Kind: reviewer
Lane: ATLAS-V03

Simplicity review of PR 113.", &[]);
        // A review beside the lane's builder and beside another review.
        assert!(conflicts(Kind::Reviewer, &simplicity, &[builder.clone(), review.clone()], &pol).is_empty());
        // A builder taking the lane is not blocked by a running review.
        assert!(conflicts(Kind::Builder, &classify("Lane: ATLAS-V03", &[]), &[review], &pol).is_empty());
        // A builder still conflicts with the lane's builder.
        assert!(conflicts(Kind::Builder, &classify("Lane: ATLAS-V03", &[]), std::slice::from_ref(&builder), &pol).iter().any(|c| c.declared));
        // Both reviews of one PR start together; a third waits for a slot.
        let items = vec![
            item("simplicity", Kind::Reviewer, "Lane: ATLAS-V03

Simplicity review of PR 113.", 3),
            item("cost", Kind::Reviewer, "Lane: ATLAS-V03

Cost review of PR 113.", 2),
            item("other", Kind::Reviewer, "Lane: ATLAS-V03

Cost review of PR 114.", 1),
        ];
        let p = propose(&items, &[builder], &|k| if k == Kind::Reviewer { 2 } else { 0 }, 9, &pol);
        assert_eq!(p.start, [0, 1]);
        assert_eq!(p.wait, [(2, "reviewer slots full".to_string())]);
    }

    #[test]
    fn the_scan_finds_whole_words_once() {
        let c = classify("Fix it right away.\nThen run `cargo nextest` and a GPU bench.\nNo rush otherwise.", &[]);
        assert_eq!((c.priority, &c.priority_src), (Priority::Urgent, &Source::Inferred("right away".into())));
        assert_eq!(c.resources().collect::<Vec<_>>(), ["compile", "gpu"]);
        // Words of one workspace are not inference words; a match inside a
        // word is not a match.
        let c = classify("Run quiet.py in the timing window. Recaptured.", &[]);
        assert!(c.uses.is_empty() && c.lane.is_none(), "{c:?}");
        assert_eq!(named_work("Build package FOO-1 next.", "build package foo-1 next."), Some("FOO-1".into()));
        assert_eq!(named_work("Start the FOO-2 lane now.", "start the foo-2 lane now."), Some("FOO-2".into()));
    }

    #[test]
    fn order_dependencies_and_slotless_items() {
        let mut items = vec![item("p2", Kind::Builder, "Uses: none", 0), item("p1", Kind::Builder, "Uses: none", 0), item("dep", Kind::Builder, "Uses: none", 0), item("orch", Kind::None, "Uses: gpu", 0)];
        items[0].order = "2".parse().ok();
        items[1].order = "1".parse().ok();
        items[2].unmet = vec!["lane RAIN-START merges".into()];
        items[3].takes_slot = false;
        let p = propose(&items, &[], &|_| 1, 1, &Policy::default());
        // Order breaks the tie; the one builder slot goes to p1; the
        // orchestrator's own item takes no slot.
        assert_eq!(p.start.iter().map(|&i| items[i].key.as_str()).collect::<Vec<_>>(), ["p1", "orch"]);
        let why: Vec<(&str, &str)> = p.wait.iter().map(|(i, r)| (items[*i].key.as_str(), r.as_str())).collect();
        assert_eq!(why, [("p2", "no agent slot is free"), ("dep", "after lane RAIN-START merges")]);
    }

    #[test]
    fn a_fractional_order_ranks_among_whole_ones_in_a_proposal() {
        let mut items = vec![item("none", Kind::Builder, "Uses: none", 5), item("ten", Kind::Builder, "Uses: none", 5), item("nine-96", Kind::Builder, "Uses: none", 5), item("nine", Kind::Builder, "Uses: none", 5), item("nine-94", Kind::Builder, "Uses: none", 5)];
        for (it, o) in items.iter_mut().skip(1).zip(["10", "9.96", "9", "9.94"]) {
            it.order = Some(o.parse().unwrap());
        }
        // Within one rank the order decides, and no order comes last though
        // it was filed first.
        let p = propose(&items, &[], &|_| 1, 1, &Policy::default());
        assert_eq!(p.start.iter().map(|&i| items[i].key.as_str()).collect::<Vec<_>>(), ["nine"]);
        assert_eq!(p.wait.iter().map(|(i, _)| items[*i].key.as_str()).collect::<Vec<_>>(), ["nine-94", "nine-96", "ten", "none"]);
    }

    #[test]
    fn an_order_is_a_decimal_as_written_and_nothing_else() {
        for ok in ["9", "9.5", "9.50", "9.94", "0", "-1", "-0.5", "+3", "007", "0.10", "123456789012345678901234567890.000000000000000000000000000001"] {
            let o: Order = ok.parse().unwrap_or_else(|()| panic!("{ok} should read"));
            assert_eq!(o.to_string(), ok, "printed as written");
        }
        for junk in ["", " ", "first", ".5", "5.", ".", "-", "+", "--1", "+-1", "1.2.3", "1,5", "9.5x", "1e3", "NaN", "nan", "inf", "-inf", " 9", "9 ", "\u{0669}"] {
            assert!(junk.parse::<Order>().is_err(), "{junk:?} should not read");
        }
    }

    #[test]
    fn orders_are_a_total_order_with_no_rounding() {
        // Strictly increasing as numbers, each against every other, with two
        // values that differ past an f64's digits.
        let ladder = ["-10", "-9.5", "-1", "-0.5", "0", "0.09", "0.1", "0.11", "0.5", "1", "1.2", "1.95", "9", "9.0000000000000000001", "9.5", "9.94", "9.96", "10", "10.5", "12.05", "100", "99999999999999999999999"];
        let parsed: Vec<Order> = ladder.iter().map(|s| s.parse().unwrap()).collect();
        for (i, a) in parsed.iter().enumerate() {
            for (j, b) in parsed.iter().enumerate() {
                assert_eq!(a.cmp(b), i.cmp(&j), "{a} against {b}");
            }
        }
        // Spellings of one number compare equal.
        for group in [&["9.5", "9.50", "09.5", "+9.5"][..], &["0", "-0", "0.0", "-0.00", "+0", "000"][..], &["10", "10.0", "010"][..]] {
            for a in group {
                for b in group {
                    let (a, b): (Order, Order) = (a.parse().unwrap(), b.parse().unwrap());
                    assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal, "{a} against {b}");
                    assert_eq!(a, b);
                }
            }
        }
        // No order sorts after every order, the largest included.
        let big: Order = "99999999999999999999999".parse().unwrap();
        assert!(Order::key(Some(&big)) < Order::key(None));
    }

    #[test]
    fn capacities_come_from_the_name_then_the_class() {
        let pol = Policy { resources: vec![("worktree".into(), 1), ("compile".into(), 0), ("db:x".into(), 2)], aging_minutes: 60 };
        assert_eq!(pol.capacity("worktree:a"), 1);
        assert_eq!(pol.capacity("db:x"), 2);
        assert_eq!(pol.capacity("db:y"), 1);
        assert_eq!(pol.capacity("compile"), 0);
        let running = vec![Holder { name: "r1".into(), kind: Kind::Builder, class: classify("Lane: a\nUses: compile, worktree:w", &[]) }];
        let c = classify("Lane: a\nUses: compile, worktree:w", &[]);
        let what = |v: Vec<Conflict>| v.into_iter().map(|c| (c.what, c.declared)).collect::<Vec<_>>();
        assert_eq!(what(conflicts(Kind::Builder, &c, &running, &pol)), [("lane a held by r1".to_string(), true), ("worktree:w held by r1".to_string(), true)]);
        assert!(conflicts(Kind::Builder, &classify("Lane: a\nUses: read-only", &[]), &running, &pol).is_empty());
        // An inferred side makes it a warning.
        let inferred = classify("Journal to lane A-ONE. Work in /w/x-worktrees/w.", &[]);
        let running = vec![Holder { name: "r2".into(), kind: Kind::Builder, class: classify("Lane: A-ONE\nUses: worktree:w", &[]) }];
        assert_eq!(what(conflicts(Kind::Builder, &inferred, &running, &pol)), [("lane A-ONE held by r2".to_string(), false), ("worktree:w held by r2".to_string(), false)]);
    }
}
