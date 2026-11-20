//! The launch queue: agent launches the agent cap refused, kept as `queued`
//! journal entries until they start.
//!
//! When the PreToolUse hook denies an Agent launch at its kind's cap, it files
//! the launch as a `queued` entry in the lane its brief names (else
//! [`LANE`]): the description as the headline, the kind, agent type and
//! session in the meta, the brief as the body. The deny reason names the key.
//! A later launch with the same description and brief retires the entry
//! (it started); so does `atlas queue drop KEY`. When an agent finishes and
//! launches are waiting, the finish notification lists the oldest of them.
//!
//! ```text
//! atlas queue [list]      the waiting launches, oldest first: key, kind, lane, age, description
//! atlas queue show KEY    one launch with its brief
//! atlas queue drop KEY    removes one without starting it
//! atlas queue release SEL counts a dead agent finished (a slot number, agent id or lane): its lane and slots are free
//! ```
//!
//! The entries live in the memory store, so the queue outlives a command
//! hook's process and a compaction, and `recall {key}` reads a brief.

use std::process::ExitCode;

use crate::checkpoint::epoch_secs;
use crate::memory::{Draft, Head, MemoryStore};
use crate::orchestrate::Kind;
use crate::Result;

/// The lane a launch is filed in when its brief names none.
pub const LANE: &str = "queue";

/// The journal entry kind.
pub const ENTRY: &str = "queued";

/// The retirement reason `atlas queue drop` writes.
const DROPPED: &str = "dropped from the queue";

/// A refused launch, as the hook saw it.
#[derive(Debug, Clone)]
pub struct Launch<'a> {
    /// Its kind.
    pub kind: Kind,
    /// The lane its brief names, if any.
    pub lane: Option<&'a str>,
    /// The Agent tool's `description`.
    pub description: &'a str,
    /// The Agent tool's `subagent_type`.
    pub subagent_type: &'a str,
    /// The brief.
    pub prompt: &'a str,
    /// The session that tried it.
    pub session: &'a str,
    /// Its classification's meta lines ([`crate::lanes::Class::meta`]), or
    /// empty.
    pub class_meta: &'a str,
}

/// A meta line's value (`key: value`).
pub fn meta_value<'a>(meta: &'a str, key: &str) -> Option<&'a str> {
    meta.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(':')).map(str::trim))
}

/// The waiting launches, oldest first, meta and titles only (bodies are
/// empty: a refused launch's body is its whole brief).
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn waiting(store: &MemoryStore) -> Result<Vec<Head>> {
    store.active_entries_no_body(ENTRY)
}

/// The launch's kind, from its meta (`kind: builder`).
pub fn kind_of(h: &Head) -> Option<Kind> {
    meta_value(&h.meta, "kind").and_then(Kind::parse)
}

/// A waiting launch's kind: its meta's, else (a row filed without one) the
/// `Kind:` line of its brief. One answer for the list and the scheduler.
fn queued_kind(store: &MemoryStore, h: &Head) -> Option<Kind> {
    if meta_value(&h.meta, "kind").is_some() {
        return kind_of(h);
    }
    store.head(&h.key).ok().flatten().and_then(|f| crate::orchestrate::brief_kind(&f.body))
}

fn sha_of(prompt: &str) -> String {
    crate::sha256::hex(prompt.as_bytes())[..16].to_string()
}

/// The header lines [`content_sha`] leaves out.
const HEADER_LINES: [&str; 4] = ["kind", "lane", "priority", "uses"];

/// A brief's content hash: the brief without its `Kind:`, `Lane:`,
/// `Priority:` and `Uses:` lines (the first of each among the first
/// [`crate::lanes::DECL_LINES`], the ones read; a later one is body text)
/// and with every run of whitespace one space, so one brief relaunched
/// under another lane, kind or priority, or re-wrapped, hashes the same.
pub fn content_sha(prompt: &str) -> String {
    let mut words: Vec<&str> = Vec::new();
    let mut seen = [false; HEADER_LINES.len()];
    for (i, l) in prompt.lines().enumerate() {
        if i < crate::lanes::DECL_LINES {
            if let Some(n) = HEADER_LINES.iter().position(|n| crate::lanes::decl(l, n).is_some()).filter(|&n| !seen[n]) {
                seen[n] = true;
                continue;
            }
        }
        words.extend(l.split_whitespace());
    }
    sha_of(&words.join(" "))
}

/// A waiting entry's content hash: its `content_sha:` meta, else (an entry
/// the hook queued before the meta existed) its brief's.
fn entry_content_sha(store: &MemoryStore, h: &Head) -> Option<String> {
    if let Some(c) = meta_value(&h.meta, "content_sha") {
        return Some(c.to_string());
    }
    meta_value(&h.meta, "brief_sha")?;
    store.head(&h.key).ok().flatten().map(|f| content_sha(&f.body))
}

/// The waiting refused launches with the same brief as `prompt` by
/// [`content_sha`], under any lane: each key with its lane.
fn duplicates(store: &MemoryStore, all: &[Head], prompt: &str) -> Vec<(String, String)> {
    let c = content_sha(prompt);
    all.iter()
        .filter(|h| !is_planned(h) && entry_content_sha(store, h).as_deref() == Some(c.as_str()))
        .map(|h| (h.key.clone(), h.scope.clone()))
        .collect()
}

/// The meta of a hook-queued launch amended to `title` and `body` (as
/// [`crate::memory::title_and_body`] made them): its `brief_sha:` follows
/// the brief alone, never a long first line kept above it, and its
/// `description:` line is that first line when the title cut it. None when
/// `meta` has no `brief_sha:` line (an entry the hook did not queue).
pub(crate) fn amended_meta(meta: &str, title: &str, body: &str) -> Option<String> {
    meta_value(meta, "brief_sha")?;
    let (head, brief) = match body.split_once('\n') {
        Some((first, rest)) if first.trim() != title && crate::memory::short_title(first) == title => (Some(first.trim()), rest),
        _ => (None, body),
    };
    let mut out: Vec<String> = meta
        .lines()
        .filter(|l| !l.starts_with("description:"))
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("brief_sha:") {
                format!("brief_sha: {}", sha_of(brief))
            } else if t.starts_with("content_sha:") {
                format!("content_sha: {}", content_sha(brief))
            } else {
                l.to_string()
            }
        })
        .collect();
    out.extend(head.map(|h| format!("description: {h}")));
    let mut m = out.join("\n");
    if meta.ends_with('\n') {
        m.push('\n');
    }
    Some(m)
}

/// Whether waiting entry `h` is this launch: the same description (whole,
/// from its `description:` line when the title cut it) and brief.
fn same(h: &Head, description: &str, prompt: &str) -> bool {
    let (title, whole) = titled(description);
    let described = match whole {
        Some(w) => meta_value(&h.meta, "description") == Some(w),
        None => meta_value(&h.meta, "description").is_none() && h.title == title,
    };
    described && meta_value(&h.meta, "brief_sha") == Some(sha_of(prompt).as_str())
}

/// A launch's title by the library rule ([`crate::memory::short_title`]),
/// and its description's whole first line when the title cut it.
fn titled(description: &str) -> (String, Option<&str>) {
    let l = description.lines().next().unwrap_or("").trim();
    if l.is_empty() {
        return ("(no description)".into(), None);
    }
    let t = crate::memory::short_title(l);
    let whole = (t != l).then_some(l);
    (t, whole)
}

/// The meta a hook-queued launch is filed with: its kind, brief sha,
/// agent type, session, classification, and the whole description when
/// the title cut it.
fn launch_meta(l: &Launch<'_>) -> String {
    let mut meta = format!("kind: {}\nbrief_sha: {}\ncontent_sha: {}", l.kind.name(), sha_of(l.prompt), content_sha(l.prompt));
    if !l.subagent_type.is_empty() {
        meta.push_str(&format!("\nsubagent_type: {}", l.subagent_type));
    }
    if !l.session.is_empty() {
        meta.push_str(&format!("\nsession: {}", l.session));
    }
    if !l.class_meta.is_empty() {
        meta.push('\n');
        meta.push_str(l.class_meta);
    }
    if let (_, Some(w)) = titled(l.description) {
        meta.push_str(&format!("\ndescription: {w}"));
    }
    meta
}

/// Files a refused launch, unless the same launch (description and brief,
/// the key its `Queue:` line names, or the same brief by [`content_sha`]
/// under any lane, description or priority, with its declared kind and its
/// uses) is already waiting. Both identity matches agree on kind: the kind
/// the prompt's own `Kind:` line declares must be the same (the same
/// brief has the same line), and a kind that only the agent type or
/// isolation gave is not compared, so the same work relaunched under
/// another agent type keeps its entry and its kind (#135); only a relaunch
/// by its `Queue:` key re-kinds an entry. A re-file by content that
/// declares a higher priority raises the waiting entry's. Returns its key
/// and whether it was filed now. Its meta records the brief's
/// [`content_sha`], which a later start under any lane matches.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn add(store: &mut MemoryStore, l: &Launch<'_>) -> Result<(String, bool)> {
    let named = queue_line(l.prompt);
    let all = waiting(store)?;
    if let Some(h) = all.iter().find(|h| same(h, l.description, l.prompt) || named.as_deref() == Some(h.key.as_str())) {
        // A hook-queued launch relaunched by its `Queue:` key as another
        // kind (a `Kind:` line added) is this launch now: its meta and brief
        // are rebuilt from it, under the same key and lane.
        let by_key = named.as_deref() == Some(h.key.as_str());
        if by_key && meta_value(&h.meta, "brief_sha").is_some() && kind_of(h) != Some(l.kind) {
            let d = Draft {
                title: h.title.clone(),
                meta: launch_meta(l),
                body: l.prompt.to_string(),
                reason: "a queued launch relaunched as another kind".into(),
                author: "hook".into(),
                client: "atlas-hook".into(),
                provenance: None,
                time: None,
            };
            let mut w = store.writer()?;
            w.revise(&h.key, None, &d)?;
            w.commit()?;
        }
        return Ok((h.key.clone(), false));
    }
    // Rule 3: the same brief by content, filed under another lane,
    // description or priority, is this launch already waiting. Its
    // declared kind and what it uses must match too: one body under two
    // `Uses:` lines (a template run for two worktrees) or two `Kind:` lines
    // is two launches.
    let uses = qmeta(l.class_meta).uses;
    let declared = crate::orchestrate::brief_kind(l.prompt);
    let declared_of = |h: &Head| store.head(&h.key).ok().flatten().map(|f| crate::orchestrate::brief_kind(&f.body));
    let dup = duplicates(store, &all, l.prompt)
        .into_iter()
        .find_map(|(k, _)| all.iter().find(|h| h.key == k && qmeta(&h.meta).uses == uses && declared_of(h) == Some(declared)));
    if let Some(h) = dup {
        raise_priority(store, h, l.class_meta)?;
        return Ok((h.key.clone(), false));
    }
    let lane = l.lane.filter(|x| !x.trim().is_empty()).unwrap_or(LANE);
    let meta = launch_meta(l);
    let (title, _) = titled(l.description);
    let d = Draft {
        title,
        meta,
        body: l.prompt.to_string(),
        reason: "agent launch refused at its kind's cap, queued by the PreToolUse hook".into(),
        author: "hook".into(),
        client: "atlas-hook".into(),
        provenance: None,
        time: None,
    };
    let mut w = store.writer()?;
    let key = w.next_key(&format!("journal:{lane}:"), 4)?;
    w.create(&key, "journal", ENTRY, lane, &d, None)?;
    w.commit()?;
    Ok((key, true))
}

/// Raises waiting entry `h`'s priority to the one `class_meta` records
/// (a re-file of its brief), when that ranks higher: its `priority:` line is
/// replaced, and its body and filing time (its wait) are kept.
fn raise_priority(store: &mut MemoryStore, h: &Head, class_meta: &str) -> Result<()> {
    let Some((new, _)) = qmeta(class_meta).priority else { return Ok(()) };
    let old = qmeta(&h.meta).priority.map_or(crate::lanes::Priority::Normal, |p| p.0);
    // Urgent sorts first.
    if new >= old {
        return Ok(());
    }
    let Some(line) = class_meta.lines().find(|l| l.starts_with("priority:")) else { return Ok(()) };
    let Some(full) = store.head(&h.key)? else { return Ok(()) };
    let mut lines: Vec<&str> = h.meta.lines().filter(|l| !l.starts_with("priority:")).collect();
    lines.push(line);
    let d = Draft {
        title: h.title.clone(),
        meta: lines.join("\n"),
        body: full.body,
        reason: "a queued launch filed again at a higher priority".into(),
        author: "hook".into(),
        client: "atlas-hook".into(),
        provenance: None,
        time: Some(h.time.clone()),
    };
    let mut w = store.writer()?;
    w.revise(&h.key, None, &d)?;
    w.commit()?;
    Ok(())
}

/// The key a brief's `Queue: KEY` line names, if any.
pub fn queue_line(brief: &str) -> Option<String> {
    brief.lines().take(40).find_map(|l| crate::lanes::decl(l, "queue")).map(|v| v.split_whitespace().next().unwrap_or("").to_string()).filter(|k| !k.is_empty())
}

/// What a started launch did to the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// Nothing waiting matched.
    None,
    /// This waiting item was retired: named by `Queue:`, or the same
    /// description and brief.
    Retired(String),
    /// The oldest waiting launch with this launch's declared lane and its
    /// kind was retired (a reworded brief; #150); `others` more still wait.
    SameLane {
        /// The retired key.
        key: String,
        /// How many other waiting launches matched and stay queued.
        others: usize,
    },
    /// Its `Queue:` line names a key that is not waiting.
    NotWaiting(String),
    /// Waiting launches with the same brief by [`content_sha`], under any
    /// lane or header, were retired: each key with its lane.
    Duplicate(Vec<(String, String)>),
}

/// Retires the waiting item a started launch is: the key its brief's
/// `Queue: KEY` line names, else the waiting launch with the same
/// description and brief, else every waiting launch with the same brief by
/// [`content_sha`] under any lane or header, else the oldest waiting launch
/// with the same lane and kind. `lane` is the launch's declared `Lane:` (an inferred or
/// missing lane matches nothing). A planned item leaves only by its
/// `Queue:` key.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn started(store: &mut MemoryStore, description: &str, prompt: &str, lane: Option<&str>, kind: Kind) -> Result<Started> {
    let all = waiting(store)?;
    let (key, out) = if let Some(k) = queue_line(prompt) {
        match all.iter().find(|h| h.key == k) {
            Some(h) => (h.key.clone(), Started::Retired(h.key.clone())),
            None => return Ok(Started::NotWaiting(k)),
        }
    } else if let Some(h) = all.iter().find(|h| same(h, description, prompt)) {
        (h.key.clone(), Started::Retired(h.key.clone()))
    } else if let dups @ [_, ..] = &duplicates(store, &all, prompt)[..] {
        let mut w = store.writer()?;
        for (k, _) in dups {
            w.retire(k, "started (the same brief under another lane or header)")?;
        }
        w.commit()?;
        return Ok(Started::Duplicate(dups.to_vec()));
    } else {
        // Oldest first, as `waiting` lists them.
        let mut matches = all.iter().filter(|h| !is_planned(h) && lane.is_some_and(|l| crate::lanes::same_lane(l, &h.scope)) && queued_kind(store, h) == Some(kind));
        let Some(h) = matches.next() else { return Ok(Started::None) };
        (h.key.clone(), Started::SameLane { key: h.key.clone(), others: matches.count() })
    };
    let mut w = store.writer()?;
    w.retire(&key, "started")?;
    w.commit()?;
    Ok(out)
}

/// Whether a queued item is planned work (`planned: true`), not a refused
/// launch.
pub fn is_planned(h: &Head) -> bool {
    meta_value(&h.meta, "planned").is_some_and(|v| v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
}

/// A queued item's meta, read as written by the hook, by `atlas queue add`
/// or by hand: each value's first word, lists split on commas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QMeta {
    /// `kind:` as written (`builder`, `orchestrator`, ...).
    pub kind: Option<String>,
    /// `priority:`, and whether it was declared: `normal (default)` and
    /// `urgent (inferred: ...)` were not.
    pub priority: Option<(crate::lanes::Priority, crate::lanes::Source)>,
    /// `lane_from:` as the hook wrote it (`declared`, `inferred: ...`).
    pub lane_from: Option<crate::lanes::Source>,
    /// `uses:`: each resource and whether it was declared (a piece marked
    /// `(inferred...)` was not).
    pub uses: Option<Vec<(String, bool)>>,
    /// `uses:` lists `read-only`, or a `read_only:` line is present.
    pub read_only: bool,
    /// `after:` conditions, as written.
    pub after: Vec<String>,
    /// `order:`.
    pub order: Option<crate::lanes::Order>,
    /// `repo:` and `branch:` (both, or neither), as the brief declared
    /// them: a writer waits while its branch is held.
    pub repo: Option<(String, String)>,
}

/// Splits on commas outside parentheses.
pub(crate) fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth <= 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out.into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// Where a meta value says it came from: `(default)`, `(inferred...)`, or
/// nothing or `(declared)` for declared.
fn source_note(v: &str) -> crate::lanes::Source {
    use crate::lanes::Source;
    if v.contains("(default)") {
        Source::Default
    } else if v.contains("(inferred") {
        Source::Inferred("filed".into())
    } else {
        Source::Declared
    }
}

/// A meta value's first word, lowercased: `normal (default)` reads `normal`.
fn first_word(v: &str) -> String {
    v.split_whitespace().next().unwrap_or("").trim_matches(['`', '"', ',']).to_ascii_lowercase()
}

/// Reads a queued item's meta.
pub fn qmeta(meta: &str) -> QMeta {
    use crate::lanes::Source;
    let first = first_word;
    let mut m = QMeta { kind: meta_value(meta, "kind").map(first).filter(|k| !k.is_empty()), ..QMeta::default() };
    m.priority = meta_value(meta, "priority").and_then(|v| crate::lanes::Priority::parse(&first(v)).map(|p| (p, source_note(v))));
    m.lane_from = meta_value(meta, "lane_from").and_then(|v| match first(v).trim_end_matches(':') {
        "declared" => Some(Source::Declared),
        "inferred" => Some(Source::Inferred("filed".into())),
        _ => None,
    });
    m.order = meta_value(meta, "order").and_then(|v| first(v).parse().ok());
    let word = |k: &str| meta_value(meta, k).and_then(|v| v.split_whitespace().next()).map(str::to_string);
    m.repo = word("repo").map(|r| crate::lanes::norm_repo(&r)).zip(word("branch"));
    m.read_only = meta_value(meta, "read_only").is_some();
    if let Some(v) = meta_value(meta, "uses") {
        let mut uses = Vec::new();
        for piece in split_top(v) {
            // The words before a `(declared)` or `(inferred ...)` note, read
            // as a `Uses:` line reads them (`sqlport 15126` is one).
            let (names, read_only) = crate::lanes::parse_uses_words(piece.split('(').next().unwrap_or(""));
            m.read_only |= read_only;
            uses.extend(names.into_iter().map(|n| (n, !piece.contains("(inferred"))));
        }
        uses.sort();
        uses.dedup();
        m.uses = Some(uses);
    }
    if let Some(v) = meta_value(meta, "after") {
        m.after = split_top(v).into_iter().filter(|x| x != "-" && !x.eq_ignore_ascii_case("none")).collect();
    }
    m
}

/// One `after` condition, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dep {
    /// A queued item's key: met once it is marked done (`atlas queue done`).
    Key(String),
    /// Free text: met only when marked (`atlas queue met KEY TEXT`).
    Condition(String),
}

/// Reads one `after` condition: a `journal:LANE:N` key, else free text.
pub fn dep(text: &str) -> Dep {
    let head: String = text.chars().take_while(|c| !c.is_whitespace() && *c != '(').collect();
    if head.starts_with("journal:") && head.matches(':').count() == 2 {
        return Dep::Key(head);
    }
    Dep::Condition(text.to_string())
}

/// The queue's marks, read once per call: the items marked done, and the
/// conditions marked met (item key, condition or `*`).
#[derive(Debug, Clone, Default)]
pub struct Marks {
    /// Keys marked done.
    pub done: std::collections::HashSet<String>,
    /// (item key, condition) marked met.
    pub met: Vec<(String, String)>,
    /// Each ticket a queue item tracks (key upper-cased), with its latest
    /// snapshot, if one was taken: what `after: ticket KEY STATUS` reads.
    pub tickets: std::collections::HashMap<String, Option<TicketSnap>>,
}

/// A ticket's latest snapshot, as the queue reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketSnap {
    /// Its status name (`Dev In Progress`).
    pub status: String,
    /// Its status category (`To Do`, `In Progress`, `Done`), if recorded.
    pub category: String,
    /// When it was fetched.
    pub fetched: String,
}

impl TicketSnap {
    /// Whether `want` names its status or its status category (case ignored).
    pub fn matches(&self, want: &str) -> bool {
        let want = want.trim();
        !want.is_empty() && (self.status.eq_ignore_ascii_case(want) || self.category.eq_ignore_ascii_case(want))
    }
}

/// Every queue item that tracks a ticket (a `ticket:` meta line), in any
/// status, oldest first: meta and titles only.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn ticket_items(store: &MemoryStore) -> Result<Vec<Head>> {
    store.entries_with_line_no_body(ENTRY, "ticket:")
}

/// The ticket items still open: waiting, or started and not yet marked
/// done (`done`, the done marks). A dropped or superseded item is closed.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn open_ticket_items(store: &MemoryStore, done: &std::collections::HashSet<String>) -> Result<Vec<Head>> {
    let mut out = Vec::new();
    for h in ticket_items(store)? {
        let open = match h.status.as_str() {
            "active" => true,
            "retired" => !done.contains(&h.key) && store.retired_reason(h.rid)?.is_some_and(|r| r.starts_with("started")),
            _ => false,
        };
        if open {
            out.push(h);
        }
    }
    Ok(out)
}

/// The latest snapshot of each tracked ticket, from `items`
/// ([`ticket_items`]): the newest `ticket_fetched` wins.
fn ticket_snaps(items: &[Head]) -> std::collections::HashMap<String, Option<TicketSnap>> {
    let mut m: std::collections::HashMap<String, Option<TicketSnap>> = std::collections::HashMap::new();
    for h in items {
        let Some(t) = crate::trackers::snapshot::ticket_of(&h.meta) else { continue };
        let v = |k: &str| meta_value(&h.meta, k).unwrap_or("").to_string();
        let snap = meta_value(&h.meta, "ticket_fetched").map(|f| TicketSnap {
            status: v("ticket_status"),
            category: v("ticket_category"),
            fetched: f.to_string(),
        });
        let slot = m.entry(t.to_ascii_uppercase()).or_default();
        if let Some(s) = snap {
            let newer = match slot.as_ref() {
                Some(old) => epoch_secs(&s.fetched).unwrap_or(i64::MIN) >= epoch_secs(&old.fetched).unwrap_or(i64::MIN),
                None => true,
            };
            if newer {
                *slot = Some(s);
            }
        }
    }
    m
}

/// Reads a typed ticket condition, `ticket KEY STATUS` (STATUS a status
/// name or category, any case): the key and the status.
pub fn ticket_condition(text: &str) -> Option<(String, String)> {
    let mut w = text.trim().splitn(2, char::is_whitespace);
    if !w.next()?.eq_ignore_ascii_case("ticket") {
        return None;
    }
    let rest = w.next()?.trim_start();
    let (key, status) = rest.split_once(char::is_whitespace)?;
    let ok = !key.is_empty() && key.len() <= 64 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let status = status.trim();
    (ok && !status.is_empty()).then(|| (key.to_ascii_uppercase(), status.to_string()))
}

impl Marks {
    /// Reads every mark: two queries over journal notes, no bodies.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Db`].
    pub fn read(store: &MemoryStore) -> Result<Marks> {
        let mut m = Marks::default();
        for meta in store.note_metas("done: ")? {
            if let Some(k) = meta_value(&meta, "done") {
                m.done.insert(k.to_string());
            }
        }
        for meta in store.note_metas("met: ")? {
            if let Some(k) = meta_value(&meta, "met") {
                m.met.push((k.to_string(), meta_value(&meta, "condition").unwrap_or("*").to_string()));
            }
        }
        m.tickets = ticket_snaps(&ticket_items(store)?);
        Ok(m)
    }

    /// Whether `item`'s condition `text` is met: a mark with the same text,
    /// or a `*` mark on the item.
    pub fn is_met(&self, item: &str, text: &str) -> bool {
        self.met.iter().any(|(k, c)| k == item && (c == text || c == "*"))
    }
}

/// The state of one `after` condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepState {
    /// Met: marked met, or its key marked done.
    Met,
    /// Not met yet.
    Waiting,
    /// Its key names no queued item, so it can never be met.
    Missing,
    /// Its key's item was dropped from the queue (`atlas queue drop`), so
    /// it can never be marked done.
    Dropped,
    /// Its key's item was superseded by this one, so it can never be marked
    /// done; the condition should name the new key.
    Superseded(String),
    /// Its key waits, directly or not, on this item.
    Cycle,
    /// A `ticket KEY STATUS` condition whose ticket no queue item tracks,
    /// so no snapshot of it can be taken.
    Untracked,
}

/// Each `after` condition of an item with its state. `waiting` is the
/// keys that still wait; a key in neither it nor the done marks is looked
/// up (an item that started and was not yet marked done waits).
pub fn dep_states(store: &MemoryStore, key: &str, after: &[String], marks: &Marks, waiting: &std::collections::HashSet<&str>) -> Vec<(String, DepState)> {
    after
        .iter()
        .map(|t| {
            let st = if marks.is_met(key, t) {
                DepState::Met
            } else if let Some((k, want)) = ticket_condition(t) {
                // Met from the ticket's latest snapshot, no mark needed.
                match marks.tickets.get(&k) {
                    None => DepState::Untracked,
                    Some(Some(s)) if s.matches(&want) => DepState::Met,
                    Some(_) => DepState::Waiting,
                }
            } else {
                match dep(t) {
                    Dep::Key(k) if marks.done.contains(&k) => DepState::Met,
                    Dep::Key(k) if waiting.contains(k.as_str()) => DepState::Waiting,
                    Dep::Key(k) => match store.head_no_body(&k).ok().flatten() {
                        Some(h) if h.kind == "journal" && h.sub == ENTRY => {
                            // Replaced, or retired without a done mark: started (it
                            // waits) or dropped.
                            if let Some(by) = h.superseded_by {
                                DepState::Superseded(by)
                            } else if h.status == "retired" && store.retired_reason(h.rid).ok().flatten().as_deref() == Some(DROPPED) {
                                DepState::Dropped
                            } else {
                                DepState::Waiting
                            }
                        }
                        _ => DepState::Missing,
                    },
                    Dep::Condition(_) => DepState::Waiting,
                }
            };
            (t.clone(), st)
        })
        .collect()
}

/// Marks the conditions that close a cycle among waiting items: item `i`
/// waits on key `k` (both waiting) and `k` leads back to `i`, that is,
/// both are in one strongly connected component. One iterative Tarjan
/// pass: O(items + edges).
fn mark_cycles(keys: &[String], states: &mut [Vec<(String, DepState)>]) {
    const NONE: usize = usize::MAX;
    let index: std::collections::HashMap<&str, usize> = keys.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();
    // Per item: (its condition's position, the waiting item it names).
    let edges: Vec<Vec<(usize, usize)>> = states
        .iter()
        .map(|s| {
            s.iter()
                .enumerate()
                .filter(|(_, (_, st))| *st == DepState::Waiting)
                .filter_map(|(c, (t, _))| if let Dep::Key(k) = dep(t) { index.get(k.as_str()).map(|&to| (c, to)) } else { None })
                .collect()
        })
        .collect();
    let n = keys.len();
    let (mut order, mut low, mut on, mut comp) = (vec![NONE; n], vec![0usize; n], vec![false; n], vec![NONE; n]);
    let (mut stack, mut next, mut comps) = (Vec::new(), 0usize, 0usize);
    // The walk: (item, its next edge to follow), reused across roots.
    let mut walk: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if order[root] != NONE {
            continue;
        }
        walk.push((root, 0));
        order[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on[root] = true;
        while let Some(&(v, e)) = walk.last() {
            if let Some(&(_, w)) = edges[v].get(e) {
                if let Some(top) = walk.last_mut() {
                    top.1 += 1;
                }
                if order[w] == NONE {
                    order[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on[w] = true;
                    walk.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(order[w]);
                }
                continue;
            }
            walk.pop();
            if let Some(&(u, _)) = walk.last() {
                low[u] = low[u].min(low[v]);
            }
            if low[v] == order[v] {
                while let Some(w) = stack.pop() {
                    on[w] = false;
                    comp[w] = comps;
                    if w == v {
                        break;
                    }
                }
                comps += 1;
            }
        }
    }
    for (i, es) in edges.iter().enumerate() {
        for &(c, to) in es {
            if comp[i] == comp[to] {
                states[i][c].1 = DepState::Cycle;
            }
        }
    }
}

/// The state of every waiting item's `after` conditions, cycles marked.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn after_states(store: &MemoryStore, heads: &[Head], metas: &[QMeta]) -> Result<Vec<Vec<(String, DepState)>>> {
    let marks = Marks::read(store)?;
    let keys: Vec<String> = heads.iter().map(|h| h.key.clone()).collect();
    let live: std::collections::HashSet<&str> = keys.iter().map(String::as_str).collect();
    let mut states: Vec<Vec<(String, DepState)>> = heads.iter().zip(metas).map(|(h, m)| dep_states(store, &h.key, &m.after, &marks, &live)).collect();
    mark_cycles(&keys, &mut states);
    Ok(states)
}

/// A queued item, waiting or not (an item that started can still be
/// marked done, and its dependents then start).
fn queued_item(store: &MemoryStore, key: &str) -> Option<Head> {
    store.head(key).ok().flatten().filter(|h| h.kind == "journal" && h.sub == ENTRY)
}

/// The lane's items that started and were not yet marked done (retired as
/// started, not superseded): `done` and `met` act on them too.
fn started_on_lane(store: &MemoryStore, lane: &str) -> Result<Vec<Head>> {
    let on = store.lane_entries_no_body(ENTRY, lane)?;
    if !on.iter().any(|h| h.status == "retired") {
        return Ok(Vec::new());
    }
    let done = Marks::read(store)?.done;
    let mut out = Vec::new();
    for h in on {
        if h.status == "retired" && h.superseded_by.is_none() && !done.contains(&h.key) && store.retired_reason(h.rid)?.is_some_and(|r| r.starts_with("started")) {
            out.push(h);
        }
    }
    Ok(out)
}

/// A KEY that is no queue key, read as a bare lane key (#182): the one
/// item of `items` on that lane (any case); `state` names what they are
/// (`waiting`). Several are listed with their
/// full keys; none says so, with the keys that contain it as suggestions.
fn by_lane(items: &[Head], key: &str, what: &str, state: &str) -> std::result::Result<Head, String> {
    let on: Vec<&Head> = items.iter().filter(|h| h.scope.eq_ignore_ascii_case(key)).collect();
    match on[..] {
        [h] => Ok(h.clone()),
        [] => {
            let k = key.to_ascii_lowercase();
            let near: Vec<&str> = items.iter().map(|h| h.key.as_str()).filter(|x| !k.is_empty() && x.to_ascii_lowercase().contains(&k)).take(3).collect();
            let hint = if near.is_empty() { " (`atlas queue` lists them with their keys)".to_string() } else { format!("; did you mean {}?", near.join(" or ")) };
            Err(format!("{key} is not {what}, and no queued item keyed {key} is {state} on that lane{hint}"))
        }
        _ => Err(format!(
            "lane {key} has {} {state} items; give the full key: {}",
            on.len(),
            on.iter().map(|h| format!("{} ({})", h.key, h.title)).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The kinds a queued item may have: the agent kinds, and `orchestrator`.
fn known_kind(k: &str) -> bool {
    Kind::parse(k).is_some() || k == "orchestrator"
}

/// The open conditions as an item's `unmet` list: missing keys and cycles
/// say so, since waiting cannot end them.
fn open(states: &[(String, DepState)]) -> Vec<String> {
    states
        .iter()
        .filter_map(|(t, st)| match st {
            DepState::Met => None,
            DepState::Waiting => Some(t.clone()),
            DepState::Missing => Some(format!("{t} (no such queued item)")),
            DepState::Dropped => Some(format!("{t} (dropped from the queue)")),
            DepState::Superseded(by) => Some(format!("{t} (superseded by {by})")),
            DepState::Cycle => Some(format!("{t} (a cycle)")),
            DepState::Untracked => Some(format!("{t} (no queue item tracks this ticket)")),
        })
        .collect()
}

/// Planned work, as `atlas queue plan` and the `journal` tool (entry
/// `queued`) file it.
#[derive(Debug, Clone, Default)]
pub struct Planned {
    /// Its lane.
    pub lane: String,
    /// Its kind as written (`builder`, `reviewer`, `research`, `orchestrator`).
    pub kind: String,
    /// Its priority, if one was given (else `normal`, read back as the
    /// default).
    pub priority: Option<crate::lanes::Priority>,
    /// Its resources (and `read-only`).
    pub uses: Vec<String>,
    /// What it waits for.
    pub after: Vec<String>,
    /// Its place in the plan.
    pub order: Option<crate::lanes::Order>,
    /// Its title.
    pub title: String,
    /// Its scope (the body; the real brief is written at launch).
    pub scope: String,
    /// Other meta lines, kept as written (`review: simplicity, cost`).
    pub extra: Vec<String>,
}

/// The meta lines a planned item is filed with.
pub fn planned_meta(p: &Planned) -> String {
    let priority = p.priority.map_or_else(|| "normal (default)".to_string(), |x| x.name().to_string());
    let mut meta = format!("kind: {}\npriority: {priority}\nuses: {}", p.kind, if p.uses.is_empty() { "none".to_string() } else { p.uses.join(", ") });
    if let Some(o) = &p.order {
        meta.push_str(&format!("\norder: {o}"));
    }
    if !p.after.is_empty() {
        meta.push_str(&format!("\nafter: {}", p.after.join(", ")));
    }
    for l in &p.extra {
        meta.push('\n');
        meta.push_str(l);
    }
    meta.push_str("\nplanned: true");
    meta
}

/// Checks a planned item before it is filed: a known kind, and every
/// `after` key naming a queued item (else it would wait forever). A new
/// item has no dependents yet, so it cannot close a cycle. A bare
/// `review pr#N` is refused unless `served` (the repositories atlas serves,
/// [`crate::pr_events::served`]) holds exactly one ([`crate::pr_events::check_wait`]);
/// an accepted one is bound to that repository, which `p` keeps in a
/// `pr_repo:` meta line (Q9, [`crate::pr_events::filed_repo`]). A `pr_repo:`
/// line the author wrote is dropped: atlas writes it.
/// The keys are read before the writer opens, so a key dropped in between
/// is not caught here; `next` and `list` then show it as dropped.
///
/// # Errors
///
/// A message naming what is wrong.
pub fn check_planned(store: &MemoryStore, p: &mut Planned, served: &[String]) -> std::result::Result<(), String> {
    if !known_kind(&p.kind) {
        return Err(format!("unknown kind {:?}: builder, reviewer, research, none or orchestrator (a tool such as blender is a resource: uses)", p.kind));
    }
    for a in &p.after {
        crate::pr_events::check_wait(a, served)?;
        if let Dep::Key(k) = dep(a) {
            if queued_item(store, &k).is_none() {
                return Err(format!("after {k}: no such queued item (`atlas queue` lists them); free text that is not a key waits for `atlas queue met`"));
            }
        }
    }
    let key = crate::pr_events::REPO_META;
    p.extra.retain(|l| l.split_once(':').is_none_or(|(k, _)| !k.trim().eq_ignore_ascii_case(key)));
    if let Some(r) = crate::pr_events::filed_repo(&p.after, served) {
        p.extra.push(format!("{key}: {r}"));
    }
    Ok(())
}

/// Reads a planned item from the meta lines an author wrote (`kind:`,
/// `priority:`, `uses:`, `after:`, `order:`; other lines are kept), with
/// its lane, title and scope.
///
/// # Errors
///
/// A message for a value that does not read.
pub fn planned_from_meta(lane: &str, title: &str, scope: &str, meta: &str) -> std::result::Result<Planned, String> {
    let mut p = Planned { lane: lane.to_string(), kind: "builder".into(), title: title.to_string(), scope: scope.to_string(), ..Planned::default() };
    for line in meta.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once(':').map_or((line, ""), |(k, v)| (k.trim(), v.trim()));
        match k.to_ascii_lowercase().as_str() {
            "kind" => p.kind = first_word(v),
            "priority" => p.priority = Some(crate::lanes::Priority::parse(&first_word(v)).ok_or_else(|| format!("priority {v:?}: urgent, normal or background"))?),
            "uses" => p.uses = split_top(v).into_iter().filter(|u| !u.eq_ignore_ascii_case("none")).collect(),
            "after" => p.after = split_top(v),
            "order" => p.order = Some(first_word(v).parse().map_err(|_| format!("order needs a number such as 9 or 9.5, not {v:?}"))?),
            "planned" => {}
            _ => p.extra.push(line.to_string()),
        }
    }
    Ok(p)
}

/// Files planned work. Returns its key.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn add_planned(store: &mut MemoryStore, p: &Planned) -> Result<String> {
    // The title rule of a journal entry: a long one is cut at a clause and
    // kept whole above the scope.
    let (title, body) = crate::memory::title_and_body(&p.title, &p.scope);
    let d = Draft {
        title,
        meta: planned_meta(p),
        body,
        reason: "planned work, filed with atlas queue plan".into(),
        author: "atlas".into(),
        client: "atlas-queue".into(),
        provenance: None,
        time: None,
    };
    let mut w = store.writer()?;
    let key = w.next_key(&format!("journal:{}:", p.lane), 4)?;
    w.create(&key, "journal", ENTRY, &p.lane, &d, None)?;
    w.commit()?;
    Ok(key)
}

/// Records that an item's condition is met (`*`: all of them), as a
/// journal note in the item's lane.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn mark_met(store: &mut MemoryStore, item: &Head, condition: &str) -> Result<String> {
    note(store, &item.scope, &format!("queue: {} after {condition} is met", item.key), &format!("met: {}\ncondition: {condition}", item.key))
}

fn note(store: &mut MemoryStore, lane: &str, title: &str, meta: &str) -> Result<String> {
    let d = Draft { title: title.into(), meta: meta.into(), body: title.into(), reason: "atlas queue".into(), author: "atlas".into(), client: "atlas-queue".into(), provenance: None, time: None };
    let mut w = store.writer()?;
    let key = w.next_key(&format!("journal:{lane}:"), 4)?;
    w.create(&key, "journal", "note", lane, &d, None)?;
    w.commit()?;
    Ok(key)
}

/// A record time as a [`std::time::SystemTime`] (`None` when unreadable or
/// before the epoch).
pub(crate) fn system_time(t: &str) -> Option<std::time::SystemTime> {
    let s = u64::try_from(epoch_secs(t)?).ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(s))
}

/// Minutes between two record times (`None` when either is unreadable).
pub(crate) fn minutes_between(time: &str, now: &str) -> Option<i64> {
    Some((epoch_secs(now)? - epoch_secs(time)?) / 60)
}

/// The waiting launches as work items. Each class is read from the meta
/// the queue wrote; only an entry filed before the meta carried one (no
/// `uses:` line) is classified from its brief. The lane is the one each
/// was filed under (none for the default lane). The `after` conditions
/// are checked against the marks read once.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn items(store: &MemoryStore, now: &str) -> Result<(Vec<Head>, Vec<crate::lanes::Item>)> {
    use crate::lanes::{Class, Source};
    let heads = waiting(store)?;
    let metas: Vec<QMeta> = heads.iter().map(|h| qmeta(&h.meta)).collect();
    let states = after_states(store, &heads, &metas)?;
    // Only a legacy row (no `uses:` meta) needs its brief, to classify it.
    let mut legacy = std::collections::HashMap::new();
    for (h, m) in heads.iter().zip(&metas) {
        if m.uses.is_none() {
            legacy.insert(h.rid, store.head(&h.key)?.map(|f| f.body).unwrap_or_default());
        }
    }
    let items = heads
        .iter()
        .zip(metas)
        .zip(states)
        .map(|((h, m), st)| {
            let mut class = match &m.uses {
                Some(u) => Class {
                    lane: None,
                    lane_src: Source::Default,
                    priority: crate::lanes::Priority::Normal,
                    priority_src: Source::Default,
                    uses: u.iter().map(|(r, d)| (r.clone(), if *d { Source::Declared } else { Source::Inferred("filed".into()) })).collect(),
                    read_only: m.read_only,
                    repo: m.repo.as_ref().map(|(r, _)| r.clone()),
                    branch: m.repo.as_ref().map(|(_, b)| b.clone()),
                },
                None => crate::lanes::classify(legacy.get(&h.rid).map_or("", String::as_str), &[]),
            };
            if h.scope != LANE {
                class.lane = Some(h.scope.clone());
                class.lane_src = if is_planned(h) { Source::Declared } else { m.lane_from.clone().unwrap_or(class.lane_src) };
            }
            if let Some((p, src)) = m.priority {
                class.priority = p;
                class.priority_src = src;
            }
            let orchestrator = m.kind.as_deref() == Some("orchestrator");
            crate::lanes::Item {
                key: h.key.clone(),
                kind: queued_kind(store, h).unwrap_or(if orchestrator { Kind::None } else { Kind::Builder }),
                class,
                waited_minutes: minutes_between(&h.time, now).unwrap_or(0).max(0),
                order: m.order,
                takes_slot: !orchestrator,
                unmet: open(&st),
            }
        })
        .collect();
    Ok((heads, items))
}

/// The running agents of a session as holders: each one's brief
/// classified, named by its description and id. `known` serves lane
/// inference, which only warns; scheduling reads declarations alone, so
/// a caller that only proposes passes none. `sub_lane` gives the sub-lane
/// a launch runs as, by its tool use id (the hook's record; its brief
/// still names the parent lane), so the next launch does not take it again.
pub fn holders(
    running: &[crate::orchestrate::Running],
    dir: &std::path::Path,
    known: &[String],
    policy: &crate::lanes::Policy,
    sub_lane: &dyn Fn(&str) -> Option<String>,
) -> Vec<crate::lanes::Holder> {
    running
        .iter()
        .map(|r| {
            let brief = crate::orchestrate::brief_of(&dir.join(format!("agent-{}.jsonl", r.id))).unwrap_or_default();
            let name = if r.description.is_empty() { r.id.clone() } else { format!("{} ({})", r.description, r.id) };
            let mut class = crate::lanes::classify_in(&brief, known, policy);
            if let Some(sub) = sub_lane(&r.tool_use_id) {
                class.lane = Some(sub);
                class.lane_src = crate::lanes::Source::Declared;
            }
            crate::lanes::Holder { name, kind: r.kind, class }
        })
        .collect()
}

/// The running agents of every session that filed a `launch` entry in the
/// last `orch.stale` window, as holders, and how many of those sessions
/// were read (#305 gap 4: `atlas queue next` without `--transcript`). That
/// spans every session in the store, not only the caller's as the
/// admission hook counts, so it can propose fewer starts than the hook
/// would admit (conservative). A session whose agents folder cannot be
/// listed is logged to stderr and skipped. An agent a
/// launch entry names takes the entry's lane (the hook's sub-lane or the
/// brief's `Lane:`), not an `agent:<id>` placeholder.
///
/// # Errors
///
/// Store errors reading the launch entries.
pub fn launch_holders(
    orch: &crate::orchestrate::OrchSettings,
    store: &MemoryStore,
    now: std::time::SystemTime,
) -> std::result::Result<(Vec<crate::lanes::Holder>, usize), String> {
    launch_holders_at(orch, store, now, &DeadProbe::machine())
}

/// [`launch_holders`] on the evidence `probe` rather than this machine's.
///
/// # Errors
///
/// Store errors reading the launch entries.
pub fn launch_holders_at(
    orch: &crate::orchestrate::OrchSettings,
    store: &MemoryStore,
    now: std::time::SystemTime,
    probe: &DeadProbe<'_>,
) -> std::result::Result<(Vec<crate::lanes::Holder>, usize), String> {
    let s = session_agents(orch, store, now, Some(probe))?;
    Ok((session_holders(orch, &s), s.sessions.len()))
}

/// The running agents of `s` as holders, each with its launch entry's lane.
fn session_holders(orch: &crate::orchestrate::OrchSettings, s: &SessionAgents) -> Vec<crate::lanes::Holder> {
    let mut out = Vec::new();
    for (dir, r) in &s.sessions {
        let mut hs = holders(r, dir, &[], &orch.lanes, &|_| None);
        for (a, h) in r.iter().zip(hs.iter_mut()) {
            if let Some(l) = s.lane_of.get(&a.id) {
                h.class.lane = Some(l.clone());
                h.class.lane_src = crate::lanes::Source::Declared;
            }
        }
        out.extend(hs);
    }
    out
}

/// The evidence that an unfinished agent died (#389,
/// [`crate::orchestrate::dead_holder`]): the machine's boot time, and
/// whether the Claude Code process an agent's launch entry names as its
/// `owner:` still runs (`None`: not provable either way).
pub struct DeadProbe<'a> {
    /// The machine's boot time; `None` when unknown.
    pub boot: Option<std::time::SystemTime>,
    /// Whether the process an `owner:` tag names still runs.
    pub owner_alive: &'a dyn Fn(&str) -> Option<bool>,
}

impl DeadProbe<'static> {
    /// This machine's boot time and process table.
    pub fn machine() -> DeadProbe<'static> {
        DeadProbe { boot: crate::platform::boot::boot_time(), owner_alive: &crate::restart::server_live }
    }
}

/// [`launch_holders`], and how many sessions the launch entries named: a
/// caller that must not act on a partial view (worktree gc) compares the
/// sessions read with the sessions found.
///
/// # Errors
///
/// Store errors reading the launch entries.
pub fn launch_holders_counted(
    orch: &crate::orchestrate::OrchSettings,
    store: &MemoryStore,
    now: std::time::SystemTime,
) -> std::result::Result<(Vec<crate::lanes::Holder>, usize, usize), String> {
    // The worktree gc acts on this view and deletes, so it releases no
    // agent on reboot or owner evidence (#389): only on a recorded finish.
    let s = session_agents(orch, store, now, None)?;
    Ok((session_holders(orch, &s), s.sessions.len(), s.named))
}

/// The cap slots held now (public #16): every running agent of the sessions
/// the launch entries name, with its lane and launch time, then every
/// launch admitted whose agent has not started
/// ([`crate::orchestrate::pending_slots`]), oldest first in each part.
///
/// # Errors
///
/// Store errors reading the launch entries.
pub fn held_slots(
    orch: &crate::orchestrate::OrchSettings,
    store: &MemoryStore,
    now: std::time::SystemTime,
) -> std::result::Result<Vec<crate::orchestrate::HeldSlot>, String> {
    Ok(held_slots_at(orch, store, now, &DeadProbe::machine())?.into_iter().map(|(_, h)| h).collect())
}

/// [`held_slots`] on the evidence `probe`, each running agent's with its
/// agents folder: the held slots in listing order, then the agents
/// released on that evidence ([`crate::orchestrate::HeldSlot::released`]),
/// which hold nothing. A row's number (`#N`, which `atlas queue release`
/// takes) is its launch's own ([`launch_slot`]), not its place here.
///
/// # Errors
///
/// Store errors reading the launch entries.
pub fn held_slots_at(
    orch: &crate::orchestrate::OrchSettings,
    store: &MemoryStore,
    now: std::time::SystemTime,
    probe: &DeadProbe<'_>,
) -> std::result::Result<Vec<(Option<std::path::PathBuf>, crate::orchestrate::HeldSlot)>, String> {
    let s = session_agents(orch, store, now, Some(probe))?;
    let slot = |id: &str, description: &str, kind, running, released| crate::orchestrate::HeldSlot {
        slot: s.slot_of.get(id).copied(),
        kind,
        lane: s.lane_of.get(id).cloned().unwrap_or_default(),
        description: if description.is_empty() { id.to_string() } else { description.to_string() },
        since: s.time_of.get(id).cloned().unwrap_or_default(),
        running,
        agent: id.to_string(),
        released,
    };
    let mut out = Vec::new();
    let mut started: Vec<&str> = Vec::new();
    for (dir, r) in &s.sessions {
        for a in r {
            started.push(&a.tool_use_id);
            out.push((Some(dir.clone()), slot(&a.id, &a.description, a.kind, true, None)));
        }
    }
    out.sort_by(|a, b| a.1.since.cmp(&b.1.since));
    // A launch whose agent is already listed running holds one slot.
    out.extend(
        crate::orchestrate::pending_slots(store, now).into_iter().filter(|(t, _)| !started.contains(&t.as_str())).map(|(_, h)| (None, h)),
    );
    let mut dead: Vec<_> = s
        .dead
        .iter()
        .map(|(dir, d)| (Some(dir.clone()), slot(&d.id, &d.description, d.kind, true, Some(d.reason))))
        .collect();
    dead.sort_by(|a, b| a.1.since.cmp(&b.1.since));
    out.extend(dead);
    Ok(out)
}

/// The held-slot listing: a heading, then one line per slot (kind, lane,
/// description, since), each launch not yet started marked so. `now` is the
/// store's time text. Empty when no slot is held.
pub fn held_text(held: &[crate::orchestrate::HeldSlot], now: &str) -> String {
    let line = |h: &crate::orchestrate::HeldSlot| {
        let lane = if h.lane.is_empty() { "-" } else { h.lane.as_str() };
        let since = if h.since.is_empty() {
            "?".to_string()
        } else {
            format!("{} ({} ago)", &h.since[..h.since.len().min(16)], age(&h.since, now))
        };
        let state = if h.running { "" } else { "  [launched, not started]" };
        let agent = if h.agent.is_empty() || h.agent == h.description { String::new() } else { format!(" (agent {})", h.agent) };
        format!("{}  lane {lane}  since {since}  {}{agent}{state}\n", h.kind.name(), h.description)
    };
    // The launch's own number (#389 review M1): the same in every view.
    let num = |h: &crate::orchestrate::HeldSlot| h.slot.map_or_else(|| "--".to_string(), |n| format!("#{n}"));
    let (dead, live): (Vec<_>, Vec<_>) = held.iter().partition(|h| h.released.is_some());
    let mut out = String::new();
    if !live.is_empty() {
        out = format!("{} agent cap slot{} held, oldest first:\n", live.len(), if live.len() == 1 { " is" } else { "s are" });
        for h in live {
            out.push_str(&format!("  {}  {}", num(h), line(h)));
        }
    }
    for h in dead {
        let why = h.released.map_or("", crate::orchestrate::DeadReason::text);
        out.push_str(&format!("  {}  released: {why}  {}", num(h), line(h)));
    }
    out
}

/// The slot number a launch entry's meta carries (`slot: N`, #389 review
/// M1): the SubagentStart hook numbers each launch when it files it, so a
/// held slot keeps its number whatever else starts, finishes or is
/// filtered out of a listing. `None` for an entry filed before slots were
/// numbered.
pub fn launch_slot(meta: &str) -> Option<u32> {
    crate::memory::import::front_value(meta, "slot").and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0)
}

/// The number the next launch takes: one past the highest among `metas`,
/// the launch entries filed in the last [`SLOT_WINDOW_FACTOR`] stale
/// windows. While any launch numbered in that window remains, no number
/// is given twice; the count starts again at 1 only after a quiet spell
/// longer than any listing that could show the old numbers.
pub fn next_launch_slot<'a>(metas: impl IntoIterator<Item = &'a str>) -> u32 {
    metas.into_iter().filter_map(launch_slot).max().map_or(1, |n| n.saturating_add(1))
}

/// How many `stale_minutes` windows back [`next_launch_slot`] looks: two,
/// so a number a listing showed (its launches are within one window) is
/// not handed out again while that listing can still be acted on.
pub const SLOT_WINDOW_FACTOR: u32 = 2;

/// The agent `atlas queue release SELECTOR` names among the slots
/// [`held_slots_at`] lists (#389): `Ok(Some((folder, id)))`, or `Ok(None)`
/// when it names no listed agent (an agent id is then looked up on disk).
/// A selector is a slot number as the listing shows it (`3` or `#3`: the
/// launch's own, [`launch_slot`]), an agent id, or a lane holding exactly
/// one started agent. Rows listed released match too: their release is
/// recorded, so the worktree gc frees them as well (#389 review S4).
///
/// # Errors
///
/// A slot number no listed agent carries, or a selector that names more
/// than one agent (each one listed).
pub fn release_target<'a>(
    sel: &str,
    slots: &'a [(Option<std::path::PathBuf>, crate::orchestrate::HeldSlot)],
) -> std::result::Result<Option<(&'a std::path::Path, &'a str)>, String> {
    // A pending launch (no folder) has no agent to release.
    let started = slots.iter().filter_map(|(d, h)| d.as_deref().filter(|_| h.running && !h.agent.is_empty()).map(|d| (d, h)));
    let n = sel.strip_prefix('#').unwrap_or(sel);
    let number = (!n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit())).then(|| n.parse::<u32>().ok()).flatten();
    let bare = sel.strip_prefix("agent-").unwrap_or(sel);
    let mut hits: Vec<(&std::path::Path, &crate::orchestrate::HeldSlot)> = Vec::new();
    for (d, h) in started {
        let named = match number {
            Some(i) => h.slot == Some(i),
            None => h.agent == bare || h.lane.eq_ignore_ascii_case(sel),
        };
        if named && !hits.iter().any(|(_, x)| x.agent == h.agent) {
            hits.push((d, h));
        }
    }
    match hits.as_slice() {
        // A number names a slot or nothing: never an agent on disk.
        [] if number.is_some() => Err(format!("no slot #{n}: no held slot carries that number now (atlas queue list numbers them)")),
        [] => Ok(None),
        [(d, h)] => Ok(Some((d, h.agent.as_str()))),
        many => Err(format!(
            "{sel:?} names {} held slots; name one by its agent id or slot number:\n{}",
            many.len(),
            many.iter().map(|(_, h)| format!("  agent {}  lane {}  {}", h.agent, if h.lane.is_empty() { "-" } else { &h.lane }, h.description)).collect::<Vec<_>>().join("\n")
        )),
    }
}

/// The running agents of the sessions the launch entries name.
struct SessionAgents {
    /// Each session folder read, with its running agents.
    sessions: Vec<(std::path::PathBuf, Vec<crate::orchestrate::Running>)>,
    /// The lane each agent's launch entry names, by agent id.
    lane_of: std::collections::HashMap<String, String>,
    /// When each agent's launch entry was written (the store's UTC time
    /// text), by agent id.
    time_of: std::collections::HashMap<String, String>,
    /// The slot number each agent's launch entry carries ([`launch_slot`]),
    /// by agent id.
    slot_of: std::collections::HashMap<String, u32>,
    /// How many sessions the launch entries named.
    named: usize,
    /// The unfinished agents left out as dead on the probe's evidence
    /// (#389), with their session folders.
    dead: Vec<(std::path::PathBuf, crate::orchestrate::DeadSlot)>,
}

fn session_agents(
    orch: &crate::orchestrate::OrchSettings,
    store: &MemoryStore,
    now: std::time::SystemTime,
    probe: Option<&DeadProbe<'_>>,
) -> std::result::Result<SessionAgents, String> {
    use crate::memory::import::front_value;
    let floor = crate::store::utc(now.checked_sub(orch.stale).unwrap_or(std::time::UNIX_EPOCH));
    let mut transcripts: Vec<String> = Vec::new();
    let mut lane_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut session_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut time_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut owner_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut slot_of: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for h in store.active_entries_no_body(crate::hooks::LAUNCH_ENTRY).map_err(|e| e.to_string())? {
        if h.time <= floor {
            continue;
        }
        let Some(t) = front_value(&h.meta, "transcript").map(str::trim).filter(|t| !t.is_empty()) else { continue };
        if !transcripts.iter().any(|x| x == t) {
            transcripts.push(t.to_string());
        }
        // The session that launched it (read as `restart` reads it): its
        // agents count from its last recorded start (#315 review S2).
        if let Some(s) = front_value(&h.meta, "session").map(str::trim).filter(|s| !s.is_empty()) {
            session_of.entry(t.to_string()).or_insert_with(|| s.to_string());
        }
        let id = front_value(&h.meta, "agent").map(str::trim).unwrap_or("");
        if !id.is_empty() && !h.scope.is_empty() && !h.scope.starts_with("agent:") {
            lane_of.insert(id.to_string(), h.scope.clone());
        }
        if !id.is_empty() {
            time_of.insert(id.to_string(), h.time.clone());
            if let Some(n) = launch_slot(&h.meta) {
                slot_of.insert(id.to_string(), n);
            }
            if let Some(o) = front_value(&h.meta, "owner").map(str::trim).filter(|o| !o.is_empty()) {
                owner_of.insert(id.to_string(), o.to_string());
            }
        }
    }
    // Each owner probed once: (tag, provably gone).
    let mut gone: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    let mut owner_gone = std::collections::HashSet::new();
    if let Some(p) = probe {
        for (id, tag) in &owner_of {
            if *gone.entry(tag.as_str()).or_insert_with(|| (p.owner_alive)(tag) == Some(false)) {
                owner_gone.insert(id.clone());
            }
        }
    }
    let mut dead = Vec::new();
    let mut sessions = Vec::new();
    for t in &transcripts {
        let dir = crate::orchestrate::subagents_dir(t);
        // One unreadable session folder (a malformed `transcript:`, a denied
        // or dropped drive) is logged and skipped, as `Hooks::restart_point`
        // does, so the other sessions still give a proposal.
        let mut agents = crate::orchestrate::Agents::new(None);
        // An agent last written before its session's start died with the
        // process before it (a crash or a closed terminal); with no
        // recorded start it counts for the whole `stale` window.
        match session_of.get(t).map(|s| crate::orchestrate::session_start(store, s)).transpose() {
            Ok(Some(Some(at))) => agents.set_opened(&dir, at),
            Ok(_) => {}
            Err(e) => eprintln!("atlas queue next: session start: {e}"),
        }
        if let Some(p) = probe {
            agents.set_dead_evidence(p.boot, owner_gone.clone());
        }
        let r = match agents.running(orch, &dir, Some(store), now) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("atlas queue next: {}: {e}", dir.display());
                continue;
            }
        };
        dead.extend(agents.dead().iter().map(|d| (dir.clone(), d.clone())));
        sessions.push((dir, r));
    }
    Ok(SessionAgents { sessions, lane_of, time_of, slot_of, named: transcripts.len(), dead })
}

/// The proposal for what runs now: free slots per kind and in total from
/// the caps.
pub fn propose(s: &crate::orchestrate::OrchSettings, items: &[crate::lanes::Item], running: &[crate::lanes::Holder]) -> crate::lanes::Proposal {
    let of = |k: Kind| running.iter().filter(|h| h.kind == k).count();
    let free = |k: Kind| s.limits(k).max.saturating_sub(of(k));
    crate::lanes::propose(items, running, &free, s.claude_limit.saturating_sub(running.len()), &s.lanes)
}

/// The line a finish notification carries while launches wait: the batch
/// that can start together, then the first few that wait, each with its
/// reason. Empty when nothing waits.
pub fn proposal_line(items: &[crate::lanes::Item], heads: &[Head], p: &crate::lanes::Proposal, max_wait: usize) -> String {
    if items.is_empty() {
        return String::new();
    }
    let title = |i: usize| heads.iter().find(|h| h.key == items[i].key).map_or("", |h| h.title.as_str());
    let desc = |i: usize| {
        let it = &items[i];
        let mut parts = vec![it.kind.name().to_string()];
        if let Some(l) = &it.class.lane {
            parts.push(format!("lane {l}"));
        }
        if it.class.priority != crate::lanes::Priority::Normal {
            parts.push(it.class.priority.name().to_string());
        }
        parts.push(title(i).to_string());
        format!("{} ({})", it.key, parts.join(", "))
    };
    let mut out = String::from("atlas queue: ");
    if p.start.is_empty() {
        out.push_str(&format!("{} waiting, none can start now", items.len()));
    } else {
        out.push_str("start together: ");
        out.push_str(&p.start.iter().map(|&i| desc(i)).collect::<Vec<_>>().join("; "));
    }
    if !p.wait.is_empty() {
        let shown: Vec<String> = p.wait.iter().take(max_wait).map(|(i, r)| format!("{} ({r})", items[*i].key)).collect();
        out.push_str(". Waiting: ");
        out.push_str(&shown.join("; "));
        let more = p.wait.len().saturating_sub(max_wait);
        if more > 0 {
            out.push_str(&format!("; {more} more (`atlas queue`)"));
        }
    }
    out.push_str(". Start each with its brief (`recall KEY`); starting one removes it from the queue.\n");
    out
}

/// How long ago a record time was, for the list (`12 min`, `3 h`, `2 d`).
pub(crate) fn age(time: &str, now: &str) -> String {
    match (epoch_secs(time), epoch_secs(now)) {
        (Some(a), Some(b)) if b >= a => {
            let d = b - a;
            if d < 3_600 {
                format!("{} min", d / 60)
            } else if d < 86_400 {
                format!("{} h", d / 3_600)
            } else {
                format!("{} d", d / 86_400)
            }
        }
        _ => "?".into(),
    }
}

/// The usage of `atlas queue`.
pub const USAGE: &str = "usage: atlas queue [list [--lane LANE] | show KEY | drop KEY | done KEY | met KEY [CONDITION] | next [--transcript PATH] | release SLOT|AGENT|LANE [--force] | release AGENT --transcript PATH [--force] | add --description TEXT [--kind KIND] FILE|- | plan --lane LANE --title TEXT [--kind KIND] [--priority P] [--uses LIST] [--after LIST] [--order N] [--scope TEXT] [--ticket KEY] | plan --from-jql QUERY [--kind KIND] [--priority P] [--uses LIST] [--after LIST] [--scope TEXT] [--order-from N] [--lane-from key|fixed:LANE] [--limit N]]";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// The first argument after the subcommand that is neither a flag nor a
/// flag's value.
fn positional(args: &[String]) -> Option<String> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a.starts_with("--") {
            it.next();
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// The lanes the store holds, for lane inference.
pub fn known_lanes(store: &MemoryStore) -> Vec<String> {
    store.lanes().map(|ls| ls.into_iter().map(|l| l.lane).filter(|l| !l.starts_with("session:")).collect()).unwrap_or_default()
}

/// `atlas queue ARGS`.
///
/// # Errors
///
/// A message for a bad argument or an unreadable store.
pub fn cli(
    args: &[String],
    store: &mut MemoryStore,
    orch: &crate::orchestrate::OrchSettings,
    served: &[String],
) -> std::result::Result<ExitCode, String> {
    let s = |e: crate::Error| e.to_string();
    let word = args.first().map(String::as_str).filter(|w| !w.starts_with("--lane")).unwrap_or("list");
    // `show`, `drop`, `done` and `met` take a KEY; `--help` there is usage.
    if matches!(word, "show" | "drop" | "done" | "met") && args.get(1).is_some_and(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let find = |store: &MemoryStore, key: Option<&String>| -> std::result::Result<Head, String> {
        let key = key.ok_or_else(|| format!("name a queued launch\n{USAGE}"))?;
        let all = waiting(store).map_err(s)?;
        match all.iter().find(|h| &h.key == key) {
            Some(h) => Ok(h.clone()),
            None => by_lane(&all, key, "a waiting launch", "waiting"),
        }
    };
    let find_any = |store: &MemoryStore, key: Option<&String>| -> std::result::Result<Head, String> {
        let key = key.ok_or_else(|| format!("name a queued item\n{USAGE}"))?;
        match queued_item(store, key) {
            Some(h) => Ok(h),
            None => {
                // The lane's started items count too: `done` and `met` act on them.
                let mut open = waiting(store).map_err(s)?;
                open.extend(started_on_lane(store, key).map_err(s)?);
                by_lane(&open, key, "a queued item, waiting or started", "open (waiting or started)")
            }
        }
    };
    match word {
        "list" => {
            // `--lane NAME` lists that lane and its sub-lanes (`NAME/2`, ...).
            let lane = flag(args, "--lane");
            let mut all = waiting(store).map_err(s)?;
            if let Some(l) = &lane {
                all.retain(|h| crate::lanes::in_lane(&h.scope, l));
            }
            let now = crate::memory::now();
            // Who holds the cap's slots, first (public #16): a refused
            // launch names them only by count.
            if orch.enabled {
                // A listing aid, as in `recall op=status`: a failed read
                // lists no slots rather than failing the queue listing.
                let mut held = held_slots(orch, store, std::time::SystemTime::now()).unwrap_or_else(|e| {
                    eprintln!("atlas queue list: held slots: {e}");
                    Vec::new()
                });
                if let Some(l) = &lane {
                    held.retain(|h| crate::lanes::in_lane(&h.lane, l));
                }
                print!("{}", held_text(&held, &now));
            }
            if all.is_empty() {
                println!("no launches are waiting");
                return Ok(ExitCode::SUCCESS);
            }
            let metas: Vec<QMeta> = all.iter().map(|h| qmeta(&h.meta)).collect();
            let states = after_states(store, &all, &metas).map_err(s)?;
            let kind_word = |m: &QMeta| match m.kind.as_deref() {
                Some(k) if known_kind(k) => k.to_string(),
                Some(k) => format!("{k}?"),
                None => "?".into(),
            };
            let (planned, refused): (Vec<usize>, Vec<usize>) = (0..all.len()).partition(|&i| is_planned(&all[i]));
            // Reviews are their own queue (journal ATLAS-V03:0095): each
            // waits only for a reviewer slot, never for a lane.
            let (reviews, refused): (Vec<usize>, Vec<usize>) = refused.into_iter().partition(|&i| queued_kind(store, &all[i]) == Some(Kind::Reviewer));
            if !refused.is_empty() {
                println!("{} launch{} waiting, oldest first:", refused.len(), if refused.len() == 1 { " is" } else { "es are" });
                for &i in &refused {
                    let h = &all[i];
                    println!("{}  {}  lane {}  {} ago  {}", h.key, kind_word(&metas[i]), h.scope, age(&h.time, &now), h.title);
                }
            }
            if !reviews.is_empty() {
                println!("{} review{} waiting for a reviewer slot, oldest first:", reviews.len(), if reviews.len() == 1 { " is" } else { "s are" });
                for &i in &reviews {
                    let h = &all[i];
                    println!("{}  {} ago  {}", h.key, age(&h.time, &now), h.title);
                }
            }
            if !planned.is_empty() {
                let mut planned = planned;
                planned.sort_by_key(|&i| (metas[i].priority.as_ref().map_or(crate::lanes::Priority::Normal, |p| p.0), crate::lanes::Order::key(metas[i].order.as_ref()), all[i].rid));
                println!("{} planned item{}, by priority then order:", planned.len(), if planned.len() == 1 { "" } else { "s" });
                for &i in &planned {
                    let (h, m) = (&all[i], &metas[i]);
                    let after = if states[i].is_empty() {
                        String::new()
                    } else {
                        let word = |st: &DepState| match st {
                            DepState::Met => "met",
                            DepState::Waiting => "waiting",
                            DepState::Missing => "no such queued item",
                            DepState::Dropped => "dropped from the queue",
                            DepState::Superseded(_) => "superseded",
                            DepState::Cycle => "a cycle",
                            DepState::Untracked => "no queue item tracks this ticket",
                        };
                        format!("  after: {}", states[i].iter().map(|(a, st)| format!("{a} [{}]", word(st))).collect::<Vec<_>>().join(", "))
                    };
                    println!(
                        "{}  {}  {}{}  {}{after}",
                        h.key,
                        kind_word(m),
                        m.priority.as_ref().map_or("normal", |p| p.0.name()),
                        m.order.as_ref().map(|o| format!(" #{o}")).unwrap_or_default(),
                        h.title
                    );
                }
            }
            let unknown: Vec<&str> = metas.iter().filter_map(|m| m.kind.as_deref()).filter(|k| !known_kind(k)).collect();
            if !unknown.is_empty() {
                let mut u = unknown.clone();
                u.sort_unstable();
                u.dedup();
                println!(
                    "{} item{} with an unknown kind (marked ?): {}. A kind is builder, reviewer, research, none or orchestrator; such an item is scheduled as a builder. A tool such as blender is a resource: `kind: builder` with `uses: blender`.",
                    unknown.len(),
                    if unknown.len() == 1 { "" } else { "s" },
                    u.join(", ")
                );
            }
            println!(
                "`atlas queue show KEY` prints one; `atlas queue next` proposes what starts now, counting the running agents of the session `--transcript PATH` names or, without it, of every session in the store that filed a launch record in the `stale_minutes` window (conservative: the admission hook counts its own session only); a launch whose brief has `Queue: KEY` takes that item off the queue; `atlas queue drop KEY` removes one, `atlas queue done KEY` marks one done, `atlas queue met KEY [CONDITION]` marks a condition met; `atlas queue release SLOT|AGENT|LANE` frees the lane and slots of an agent that died without its finish recorded, named by its slot number above (the launch's own, the same in every listing), its agent id, or a lane one started agent holds; with `--transcript PATH` it takes an agent id in that session only. KEY is an item's full key, or its lane when one item is open there (waiting, or started and not yet done; `show` and `drop` take waiting items only)."
            );
            Ok(ExitCode::SUCCESS)
        }
        "show" => {
            let h = find(store, args.get(1))?;
            let body = store.head(&h.key).map_err(s)?.map(|f| f.body).unwrap_or_default();
            println!("{}  {}  lane {}  {}\n{}\n\n{}", h.key, kind_of(&h).map_or("?", Kind::name), h.scope, h.title, h.meta, body);
            Ok(ExitCode::SUCCESS)
        }
        "drop" => {
            let h = find(store, args.get(1))?;
            let mut w = store.writer().map_err(s)?;
            w.retire(&h.key, DROPPED).map_err(s)?;
            w.commit().map_err(s)?;
            println!("dropped {} ({})", h.key, h.title);
            Ok(ExitCode::SUCCESS)
        }
        "next" => {
            let (heads, items) = items(store, &crate::memory::now()).map_err(s)?;
            if items.is_empty() {
                println!("no launches are waiting");
                return Ok(ExitCode::SUCCESS);
            }
            let running;
            if let Some(t) = flag(args, "--transcript") {
                let dir = crate::orchestrate::subagents_dir(&t);
                let r = crate::orchestrate::Agents::new(None)
                    .running(orch, &dir, Some(&*store), std::time::SystemTime::now())
                    .map_err(|e| format!("{}: {e}", dir.display()))?;
                running = holders(&r, &dir, &[], &orch.lanes, &|_| None);
            } else {
                let (r, sessions) = launch_holders(orch, store, std::time::SystemTime::now())?;
                running = r;
                if sessions == 0 {
                    println!("(no running agents were found from launch records; --transcript names one session)");
                } else {
                    println!(
                        "(running agents from the launch records of {sessions} session{}; --transcript names one session)",
                        if sessions == 1 { "" } else { "s" }
                    );
                }
            }
            let p = propose(orch, &items, &running);
            print!("{}", proposal_line(&items, &heads, &p, usize::MAX));
            Ok(ExitCode::SUCCESS)
        }
        "add" => {
            let description = flag(args, "--description").ok_or_else(|| format!("add needs --description\n{USAGE}"))?;
            let kind = match flag(args, "--kind") {
                Some(k) => Kind::parse(&k).ok_or_else(|| format!("unknown kind {k:?}: builder, reviewer, research or none"))?,
                None => crate::orchestrate::brief_kind_or(orch, ""),
            };
            let src = positional(args).ok_or_else(|| format!("add needs a brief file, or - for stdin\n{USAGE}"))?;
            let brief = if src == "-" {
                let mut b = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut b).map_err(|e| e.to_string())?;
                b
            } else {
                std::fs::read_to_string(&src).map_err(|e| format!("{src}: {e}"))?
            };
            let kind = if flag(args, "--kind").is_some() { kind } else { crate::orchestrate::brief_kind_or(orch, &brief) };
            let known = known_lanes(store);
            let class = crate::lanes::classify_in(&brief, &known, &orch.lanes);
            let meta = class.meta();
            let launch = Launch { kind, lane: class.lane.as_deref(), description: &description, subagent_type: "", prompt: &brief, session: "", class_meta: &meta };
            let (key, new) = add(store, &launch).map_err(s)?;
            println!("{} {key} ({}, lane {}, {})", if new { "queued" } else { "already queued as" }, kind.name(), class.lane.as_deref().unwrap_or(LANE), class.priority.name());
            Ok(ExitCode::SUCCESS)
        }
        "plan" => {
            let need = |n: &str| flag(args, n).ok_or_else(|| format!("plan needs {n}\n{USAGE}"));
            let lane = need("--lane")?;
            let kind = flag(args, "--kind").unwrap_or_else(|| "builder".into()).to_ascii_lowercase();
            let priority = match flag(args, "--priority") {
                Some(p) => Some(crate::lanes::Priority::parse(&p).ok_or_else(|| format!("unknown priority {p:?}: urgent, normal or background"))?),
                None => None,
            };
            let list = |n: &str| flag(args, n).map(|v| split_top(&v)).unwrap_or_default();
            let order = flag(args, "--order").map(|o| o.parse::<crate::lanes::Order>().map_err(|()| format!("--order needs a number such as 9 or 9.5, not {o:?}"))).transpose()?;
            let mut p = Planned { lane, kind, priority, uses: list("--uses"), after: list("--after"), order, title: need("--title")?, scope: flag(args, "--scope").unwrap_or_default(), extra: Vec::new() };
            if let Some(t) = flag(args, "--ticket") {
                // The ticket this item tracks (atlas track snapshot).
                let t = t.trim();
                if t.is_empty() || t.len() > 64 || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                    return Err(format!("--ticket {t:?}: a ticket key such as PROJ-123"));
                }
                p.extra.push(format!("ticket: {t}"));
            }
            check_planned(store, &mut p, served)?;
            let key = add_planned(store, &p).map_err(s)?;
            println!("planned {key} ({}, {}{})", p.kind, p.priority.map_or("normal", crate::lanes::Priority::name), p.order.as_ref().map(|o| format!(", order {o}")).unwrap_or_default());
            Ok(ExitCode::SUCCESS)
        }
        "done" => {
            let h = find_any(store, args.get(1))?;
            note(store, &h.scope, &format!("queue: {} done", h.key), &format!("done: {}", h.key)).map_err(s)?;
            if h.status == "active" {
                let mut w = store.writer().map_err(s)?;
                w.retire(&h.key, "done").map_err(s)?;
                w.commit().map_err(s)?;
            }
            println!("done {} ({})", h.key, h.title);
            Ok(ExitCode::SUCCESS)
        }
        "met" => {
            let h = find_any(store, args.get(1))?;
            let cond = if args.len() > 2 { args[2..].join(" ") } else { "*".to_string() };
            let key = mark_met(store, &h, &cond).map_err(s)?;
            println!("{} after {cond}: met ({key})", h.key);
            Ok(ExitCode::SUCCESS)
        }
        "release" => {
            // `release AGENT [--force] [--transcript PATH]` (#204).
            let (mut id, mut force, mut transcript) = (None, false, None);
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-h" | "--help" => {
                        println!("{USAGE}");
                        return Ok(ExitCode::SUCCESS);
                    }
                    "--force" => force = true,
                    "--transcript" => transcript = Some(it.next().ok_or_else(|| format!("--transcript needs a path\n{USAGE}"))?.clone()),
                    f if f.starts_with('-') => return Err(format!("unknown release flag {f:?}\n{USAGE}")),
                    a if id.is_none() => id = Some(a.to_string()),
                    a => return Err(format!("release takes one slot, agent id or lane, not also {a:?}\n{USAGE}")),
                }
            }
            let id = id.ok_or_else(|| format!("name a held slot: its number in `atlas queue list`, its agent id or its lane\n{USAGE}"))?;
            let mut bare = id.strip_prefix("agent-").unwrap_or(&id).to_string();
            // A slot number, an agent id or a lane among the held slots
            // (#389); else an agent id looked up on disk, as before. With
            // `--transcript` the selector is an agent id in that session.
            let mut found = None;
            if transcript.is_none() {
                // The slots are an aid: a failed read leaves the id lookup.
                let slots = held_slots_at(orch, store, std::time::SystemTime::now(), &DeadProbe::machine()).unwrap_or_else(|e| {
                    eprintln!("atlas queue release: held slots: {e}");
                    Vec::new()
                });
                if let Some((d, a)) = release_target(&id, &slots)? {
                    found = Some(d.to_path_buf());
                    bare = a.to_string();
                }
            }
            let bare = bare.as_str();
            let dir = match (found, transcript) {
                (Some(d), _) => d,
                (None, Some(t)) => {
                    let dir = crate::orchestrate::subagents_dir(&t);
                    if !dir.join(format!("agent-{bare}.meta.json")).is_file() {
                        return Err(format!(
                            "no agent {bare} in {}: --transcript takes the session transcript (<session>.jsonl, beside the subagents folder that holds agent-{bare}.meta.json), not an agent's own",
                            dir.display()
                        ));
                    }
                    dir
                }
                (None, None) => {
                    let projects = crate::chain::projects_dir().ok_or("no home folder is known (set HOME, USERPROFILE or CLAUDE_CONFIG_DIR)")?;
                    crate::orchestrate::find_agent_dir(&projects, bare)
                        .ok_or_else(|| format!("no held slot, agent or lane {bare} (atlas queue list shows the held slots), and no agent {bare} under {}; name its session with --transcript PATH", projects.display()))?
                }
            };
            println!("{}", crate::orchestrate::release(store, &dir, bare, force, std::time::SystemTime::now())?);
            Ok(ExitCode::SUCCESS)
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        w => Err(format!("unknown queue command {w:?}\n{USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_content_hash_ignores_the_header_lines_and_whitespace() {
        let a = "Kind: research\nLane: ATLAS-TOOLING\nPriority: normal\nUses: none (read-only)\n\n# Measure it\n\nRead the logs; build nothing.";
        let b = "- **Kind:** builder\nLane: ATLAS-HOOK-VALUE\nUses: compile\n\n#  Measure it\nRead the logs;\n   build nothing.\n";
        assert_eq!(content_sha(a), content_sha(b));
        assert_eq!(content_sha(a), content_sha("# Measure it Read the logs; build nothing."));
        // The body is the content: a changed word is another brief.
        assert_ne!(content_sha(a), content_sha(&a.replace("nothing", "everything")));
        // Header lines count only where they are read (the first 40 lines).
        let late = format!("{}\nLane: X", "body line\n".repeat(45));
        assert_ne!(content_sha(&late), content_sha(&format!("{}\nLane: Y", "body line\n".repeat(45))));
        // A `Queue:` line is not a header line.
        assert_ne!(content_sha("Queue: journal:x:0001\nDo it."), content_sha("Do it."));
    }

    #[test]
    fn ages_read_minutes_hours_and_days() {
        assert_eq!(age("2026-11-04T12:00:00Z", "2026-11-04T12:42:10Z"), "42 min");
        assert_eq!(age("2026-11-04T09:00", "2026-11-04T12:00"), "3 h");
        assert_eq!(age("2026-10-31T23:00:00Z", "2026-11-04T00:00:00Z"), "3 d");
        assert_eq!(age("garbage", "2026-11-04T00:00:00Z"), "?");
    }

    #[test]
    fn planned_meta_reads_back_as_written() {
        let p = planned_from_meta("L", "t", "", "kind: Reviewer (by hand)
priority: normal (default)
uses: gpu, compile
order: 4 (first)
review: simplicity, cost").unwrap();
        assert_eq!((p.kind.as_str(), p.priority, p.order.clone()), ("reviewer", Some(crate::lanes::Priority::Normal), "4".parse().ok()));
        assert_eq!(p.extra, ["review: simplicity, cost"]);
        // What the queue writes, read again, is the same item.
        let q = planned_from_meta("L", "t", "", &planned_meta(&Planned { priority: None, ..p.clone() })).unwrap();
        assert_eq!((q.kind, q.priority, q.uses, q.order), (p.kind, Some(crate::lanes::Priority::Normal), p.uses, p.order));
        assert!(planned_from_meta("L", "t", "", "priority: soon").is_err());
    }

    #[test]
    fn a_fractional_order_is_read_filed_and_read_back_as_written() {
        // The reader, with a note after the number as a hand-written entry has.
        assert_eq!(qmeta("order: 9.5 (after the body pass)").order.map(|o| o.to_string()).as_deref(), Some("9.5"));
        assert_eq!(qmeta("order: 9.50").order.map(|o| o.to_string()).as_deref(), Some("9.50"));
        // Junk in the store still reads as no order.
        assert!(qmeta("order: first").order.is_none());
        // The writer takes what the reader reads, and writes it unchanged.
        let p = planned_from_meta("L", "t", "", "order: 9.94").unwrap();
        assert!(planned_meta(&p).lines().any(|l| l == "order: 9.94"), "{}", planned_meta(&p));
        assert_eq!(qmeta(&planned_meta(&p)).order, p.order);
        for junk in ["first", "9.5.1", ".5", "NaN", "inf", "1e3"] {
            let e = planned_from_meta("L", "t", "", &format!("order: {junk}")).unwrap_err();
            assert!(e.contains("order needs a number") && e.contains(&format!("{junk:?}")), "{e}");
        }
    }

    #[test]
    fn a_meta_value_is_read_by_key() {
        let m = "kind: builder\nbrief_sha: abc\nsubagent_type: general-purpose";
        assert_eq!(meta_value(m, "kind"), Some("builder"));
        assert_eq!(meta_value(m, "subagent_type"), Some("general-purpose"));
        assert_eq!(meta_value(m, "session"), None);
    }
}
