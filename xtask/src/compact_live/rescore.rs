//! `compact-live --rescore [--job DIR]`: the saved jobs scored on the
//! three-way split of Brandon's accuracy rule (journal 0144), spending
//! nothing. A fact of the cut (the [`atlas::compact::facts_at`] rule) is
//!
//! * (a) in the summary: the summary or the atlas card holds it;
//! * (b) dropped but recoverable: one hop from the summary or card brings
//!   back text that holds it, run for real against the job's state as of
//!   its cut;
//! * (c) lost: neither.
//!
//! The one hop is `recall key=K` for a record key written literally in the
//! summary or card, or the `UserPromptSubmit` hook's recall on the job's
//! turn prompt (rebuilt from the resumed transcript, checked equal to the
//! recording's), plus one `recall key=K` of a key that recall listed. The
//! scorer never makes up a query. Facts match as in (a), by substring.
//!
//! The state as of the cut: the job's saved memory store with every
//! version written after the cut dropped ([`super::prune_store_keeping`]),
//! except the atlas arm's own checkpoint (its card, read from that pruned
//! store), over an empty index with no sources.
//!
//! No path reads the index: the hook's recall reads only the memory store
//! (`recall::candidates` on the hook's store), [`keys_named`] admits
//! memory-store keys only, and `recall key=` is a memory tool. So a run
//! with the saved index equals the run without it by construction, and the
//! indexed variant is not scored. If any path ever reads the index, the
//! brief's twice-reporting rule applies again: the saved index's sections
//! come from a later snapshot and cannot be cut to the time, so (b) is then
//! reported with and without it, and the verdict read on the figure without.
//!
//! The chain path (journals 0202-0208): the atlas arm's card is rebuilt
//! from the job's pre-cut transcript (its first `keep` lines, written to
//! the rescore folder under the job's session id; the saved card's
//! `tools:` line is dropped), and every chain key it or the summary prints
//! is resolved by [`atlas::chain::resolve`] against that pre-cut copy only,
//! never the index or anything past the cut. A reply is hop 1; a chain key
//! inside a hop-1 reply is hop 2 and counts only when it names an older
//! sub-segment than the cut's own. Each fact's hop is kept, and every split
//! is also read with no reply's `digest:` line credited (0199). Facts that
//! no record's redacted text holds are excluded for both arms first, and
//! counted.
//!
//! Beside the split, two columns on the summary and card only: the facts
//! of the rule before the authored-fields exclusion (`old rule`), and the
//! authored facts themselves (Edit/Write/MultiEdit bodies and TodoWrite
//! content, which share the excluded field names), by tool.
//!
//! Every fact is also placed with the card's entity text stripped (keys
//! only, 0186), with and without digest credit, and an (a) the card holds
//! but the summary does not is counted apart. Each job keeps its
//! summary.txt (and the atlas arm its card.txt) for the token counter.
//!
//! The bars come first per pair ([`print_bars`]): a pair with no facts is
//! no data, never a pass. Then the verdict per compaction point: the rule fixed before the run
//! (atlas wins accuracy when its (c) is no higher than plain's and its
//! (a+b) at least as high), which is the verdict of record; then, per pair,
//! each arm's median (c), its spread and its fact count, whether the pair
//! separates beyond the spread, and a resolution line added after the run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use atlas::compact::FactRule;
use atlas::json::Value;

use super::{boundary_lines, job_config_path, job_dir, measure, next_prompt_in, open_store, read_json, transcript, Job, Measured, Opts, Run};

/// Where a fact landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Summary,
    Recoverable,
    Lost,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Summary => "a",
            Class::Recoverable => "b",
            Class::Lost => "c",
        }
    }
}

/// One fact's place, and the call that recovered it: `class` on the paths
/// the run had (keys the summary or card names; its jobs registered no
/// `UserPromptSubmit` hook, journal 0150), `live` with the live prompt
/// hook's recall and the keys it lists added.
#[derive(Debug, Clone)]
struct Row {
    fact: String,
    tool: String,
    class: Class,
    via: String,
    live: Class,
    live_via: String,
    /// The hops a chain key path took (journal 0202): 1 for a key the
    /// summary or card prints, 2 for a key in such a reply (older
    /// sub-segments only). `None` off the chain path.
    hop: Option<u8>,
    /// The class on the run's paths with no digest line credited (0199).
    class_nd: Class,
    /// An (a) the card holds and the summary does not (0186).
    a_card: bool,
    /// The class with the card's entity text stripped (keys only), with
    /// and without digest credit.
    class_k: Class,
    class_k_nd: Class,
}

/// One job's split.
#[derive(Debug, Clone, Default)]
struct Split {
    rows: Vec<Row>,
    /// The bytes of the text the hook's recall injected on the turn prompt
    /// (0 when the point ran no turn), and the keys that text listed.
    hook_bytes: usize,
    hook_keys: Vec<String>,
    /// The keys the summary and card name literally.
    named_keys: Vec<String>,
    /// Facts excluded before scoring: every occurrence is redacted (0199).
    excluded: usize,
    /// The largest chain reply, in estimated tokens (bar 5), and the chain
    /// keys resolved.
    chain_max: usize,
    chain_keys: usize,
    /// The card as rebuilt (the atlas arm): bytes and estimated tokens.
    card_bytes: usize,
    card_est: usize,
    /// Each chain reply resolved: its key, hop, uncapped estimate, and
    /// whether the cap cuts it.
    replies: Vec<(String, u8, usize, bool)>,
    /// Failed calls that do not fail the job, each as (kind, detail): a
    /// recall, or a dangling chain key (a real loss, listed).
    errors: Vec<(String, String)>,
    /// Rows held only inside a key token: (a) in the summary or card, and
    /// (b) in the chain replies.
    a_inside: usize,
    b_inside: usize,
}

impl Split {
    /// The share of facts whose class on the run's paths passes `f`.
    fn rate(&self, f: impl Fn(Class) -> bool) -> f64 {
        self.share(|r| r.class, f)
    }

    /// The same with the live hook's paths added.
    fn live_rate(&self, f: impl Fn(Class) -> bool) -> f64 {
        self.share(|r| r.live, f)
    }

    /// The share of rows whose class (`of`) passes `f`. With no facts,
    /// everything is in the summary.
    fn share(&self, of: impl Fn(&Row) -> Class, f: impl Fn(Class) -> bool) -> f64 {
        if self.rows.is_empty() {
            return if f(Class::Summary) { 1.0 } else { 0.0 };
        }
        self.frac(|r| f(of(r)))
    }

    /// The share of rows for which `f` holds; 0 with no facts.
    fn frac(&self, f: impl Fn(&Row) -> bool) -> f64 {
        if self.rows.is_empty() {
            return 0.0;
        }
        self.rows.iter().filter(|r| f(r)).count() as f64 / self.rows.len() as f64
    }
}

/// One job rescored.
#[derive(Debug, Clone)]
struct Rescored {
    name: String,
    arm: String,
    at: String,
    point: usize,
    split: Split,
    /// The old rule's facts: (kept in summary or card, all).
    old: (usize, usize),
    /// Authored facts: (kept, all, all per tool).
    authored: (usize, usize, BTreeMap<String, usize>),
    measured: Option<Measured>,
    /// The summary's bytes and estimated tokens (bar 3 is read in counted
    /// tokens, out of band, from the kept summary.txt and card.txt).
    summary_bytes: usize,
    summary_est: usize,
}

/// The text a recall answer carries: every string value, one per line.
fn texts(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(a) => a.iter().for_each(|x| texts(x, out)),
        Value::Object(m) => m.values().for_each(|x| texts(x, out)),
        _ => {}
    }
}

/// Whether `store` holds a record keyed `k`.
fn exists(store: &atlas::memory::MemoryStore, k: &str) -> bool {
    store.conn().query_row("SELECT 1 FROM records WHERE key = ?1", [k], |r| r.get::<_, i64>(0)).is_ok()
}

/// The record keys a text writes literally, in order, each once: tokens of
/// key characters, with a `@hash` suffix and wrapping punctuation dropped,
/// that name a record of `store`.
fn keys_named(text: &str, store: &atlas::memory::MemoryStore) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.' | '@' | '/'))) {
        let k = raw.split('@').next().unwrap_or("").trim_matches(|c: char| matches!(c, '.' | ':' | '-' | '/'));
        if k.len() < 3 || out.iter().any(|x| x == k) {
            continue;
        }
        if exists(store, k) {
            out.push(k.to_string());
        }
    }
    out
}

/// A card with its entity text stripped (0186): only its keyed tokens
/// (`key@hash`, chain keys included), one per line, so a fact it scores
/// as (a) is one a key itself spells.
fn keys_only(card: &str) -> String {
    let mut out = String::new();
    for w in card.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | ';')) {
        let w = w.trim_matches(|c: char| matches!(c, '.' | ':' | '-'));
        if w.contains('@') {
            out.push_str(w);
            out.push('\n');
        }
    }
    out
}

/// The job's state as of its cut in `out`: the memory store pruned to the
/// cut (keeping `keep`, the atlas arm's checkpoint lane), and a
/// configuration with no sources over a new empty index (no path reads the
/// index; see the module doc). Returns the configuration's path.
fn state_as_of(job: &Path, out: &Path, cut: &str, keep: &str) -> Result<PathBuf, String> {
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    if out.exists() {
        std::fs::remove_dir_all(out).map_err(|e| err(out, e))?;
    }
    std::fs::create_dir_all(out).map_err(|e| err(out, e))?;
    let mem = super::copy_db(&job.join("state").join("memory.sqlite"), out)?;
    super::prune_store_keeping(&mem, cut, keep)?;
    let cfg = job_config_path(job);
    let text = std::fs::read_to_string(&cfg).map_err(|e| err(&cfg, e))?;
    let mut v = atlas::json::parse(&text).map_err(|_| format!("{} is not JSON", cfg.display()))?;
    let slash = |p: &Path| Value::String(p.to_string_lossy().replace('\\', "/"));
    let root = v.as_object_mut().ok_or("the job's config is not an object")?;
    root.insert("db".into(), slash(&out.join("empty-index.sqlite")));
    // The vaults stay named (a server needs one) over the empty index,
    // which is never scanned (see `server`).
    root.insert("sources".into(), Value::Array(Vec::new()));
    root.remove("code");
    let mut memory = root.get("memory").and_then(Value::as_object).cloned().unwrap_or_default();
    memory.insert("store".into(), slash(&mem));
    memory.insert("export_dir".into(), slash(&out.join("exports")));
    memory.insert("snapshot_dir".into(), slash(&out.join("snapshots")));
    root.insert("memory".into(), Value::Object(memory));
    let path = out.join("atlas.json");
    std::fs::write(&path, atlas::json::to_string_pretty(&v)).map_err(|e| err(&path, e))?;
    Ok(path)
}

/// A server over the state's index and store, never rescanned: a rescan
/// would index today's files into the state as of the cut. `recall` is a
/// memory tool, so no call made here rescans either.
fn server(c: &atlas::config::Config, db: &Path) -> Result<atlas::tools::Server, String> {
    let vaults: Vec<(String, PathBuf)> = c.vaults.iter().map(|v| (v.name.clone(), v.path.clone())).collect();
    let mut setup = atlas::store::AtlasSetup::from_config(c).map_err(|e| e.to_string())?;
    setup.watch = false;
    let store = atlas::paths::Vaults::new(&vaults).and_then(|v| atlas::store::Store::open_with(db, v, setup)).map_err(|e| e.to_string())?;
    let mut server = atlas::tools::Server::new(store);
    server.set_memory(atlas::memory::settings::Settings::from_config(Some(c), db).map_err(|e| e.to_string())?);
    Ok(server)
}

/// One chain reply read: its key, hop, the reply as `recall` returns it
/// and the same without the reply's own digest line (0199).
type ChainReply = (String, u8, String, String);

/// The chain path's BFS (journal 0202 hop rule): the replies of the chain
/// keys `have` prints are hop 1; keys in those replies are hop 2 and count
/// only when [`atlas::chain::older`] than the sub-segment starting at
/// `cur` (so a current group's `next:` key is not read). Each key is
/// resolved once. A dangling key is a real loss (a misquoted or stale
/// key): it is listed in `errors` and reads nothing. Any other miss (no
/// transcript) fails the job. Each reply's uncapped estimate and whether
/// the cap cuts it go to `replies`.
fn chain_bfs(
    have: &str,
    cur: u64,
    mut resolve: impl FnMut(&str) -> Result<atlas::chain::Reply, atlas::chain::Miss>,
    replies: &mut Vec<(String, u8, usize, bool)>,
    errors: &mut Vec<(String, String)>,
) -> Result<Vec<ChainReply>, String> {
    let mut chain = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut frontier = atlas::chain::written_keys(have);
    for hop in 1..=2u8 {
        let mut next = Vec::new();
        for k in frontier {
            if seen.contains(&k) || (hop == 2 && !atlas::chain::older(&k, cur)) {
                continue;
            }
            seen.push(k.clone());
            match resolve(&k) {
                Ok(r) => {
                    replies.push((k.clone(), hop, atlas::chain::est(&r.text), r.is_cut()));
                    let t = r.capped();
                    next.extend(atlas::chain::written_keys(&t));
                    chain.push((k, hop, t, r.capped_without_digest()));
                }
                Err(m @ atlas::chain::Miss::Dangling(_)) => errors.push(("resolve dangling".into(), format!("{k}: {m}"))),
                Err(m) => return Err(format!("chain key {k}: {m}")),
            }
        }
        frontier = next;
    }
    Ok(chain)
}

/// `text` with every key token blanked: each match of
/// [`atlas::chain::written_keys`] or [`crate::memory_eval::keys_in`], where it
/// stands as a whole token, becomes one space. A fact that matched only
/// inside a key does not match the result.
fn blank_keys(text: &str) -> String {
    let mut ks = atlas::chain::written_keys(text);
    ks.extend(crate::memory_eval::keys_in(text));
    ks.sort_by_key(|k| std::cmp::Reverse(k.len()));
    ks.dedup();
    let key_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '#' | '@');
    let mut out = text.to_string();
    for k in ks.iter().filter(|k| !k.is_empty()) {
        let mut next = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(k.as_str()) {
            let (before, after) = (rest[..i].chars().next_back(), rest[i + k.len()..].chars().next());
            next.push_str(&rest[..i]);
            if before.is_some_and(key_char) || after.is_some_and(key_char) {
                next.push_str(k);
            } else {
                next.push(' ');
            }
            rest = &rest[i + k.len()..];
        }
        next.push_str(rest);
        out = next;
    }
    out
}

/// The inputs a job's split needs.
struct Inputs {
    /// The job's name, for the pre-embed line.
    name: String,
    session: String,
    cwd: PathBuf,
    have: String,
    /// None when the point ran no turn (the recording has no prompt after it).
    prompt: Option<String>,
    /// The resumed transcript: the hook only logs its path.
    tx: PathBuf,
    /// The summary alone, and the summary with the card's keys only.
    summary: String,
    have_k: String,
    facts: Vec<(String, String)>,
    /// The chain path: the pre-cut transcript's folder, and where its
    /// current sub-segment starts (the atlas arm only).
    chain: Option<(PathBuf, u64)>,
    excluded: usize,
    /// The vector cache shared by the run's jobs (a job's store has the
    /// same records as many others; see `embed_all_pending_cached`).
    vectors: PathBuf,
}

/// The split under the state `cfg` names (see the module doc).
fn split(inp: &Inputs, cfg: &Path) -> Result<Split, String> {
    let c = atlas::config::Config::load(cfg).map_err(|e| format!("{}: {e}", cfg.display()))?;
    let db = c.db.clone().ok_or("the rescore config names no db")?;
    let settings = atlas::memory::settings::Settings::from_config(Some(&c), &db).map_err(|e| e.to_string())?;
    // The prune rolls records back to versions with no vector, so the hooks
    // would start a background embedder that writes vectors while the
    // recall below runs, and is never joined. Embedded here first, nothing
    // is pending when the hooks open, no embedder starts, and the recall is
    // the same on every rescore. Vectors already made for the same record
    // text (by any job) come from the cache.
    let t0 = std::time::Instant::now();
    if let Some(n) = crate::memory_eval::embed_all_pending_cached(&settings, &inp.vectors)? {
        println!("{}: embedded {n} pending record vectors before the hooks opened, in {:.1} s", inp.name, t0.elapsed().as_secs_f64());
    }
    // The hook's recall on the turn prompt, as the command hook answers it.
    let mut hooks = atlas::hooks::Hooks::open(settings).map_err(|e| e.to_string())?;
    hooks.set_command_hook();
    let ev = atlas::hooks::HookEvent::from_value(&atlas::json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": inp.session.clone(),
        "transcript_path": inp.tx.to_string_lossy().into_owned(),
        "cwd": inp.cwd.to_string_lossy().into_owned(),
        "prompt": inp.prompt.clone().unwrap_or_default(),
    }));
    let hook = if inp.prompt.is_some() { hooks.handle(&ev).context } else { String::new() };
    let store = hooks.store();
    let named = keys_named(&inp.have, store);
    let hook_keys: Vec<String> = crate::memory_eval::keys_in(&hook).into_iter().filter(|k| exists(store, k)).collect();
    drop(hooks);
    // One read per key, through the server the arm had.
    let mut server = server(&c, &db)?;
    let mut errors: Vec<(String, String)> = Vec::new();
    let mut read = |k: &str| -> String {
        let mut t = String::new();
        match server.call("recall", &atlas::json!({"key": k})) {
            Ok(v) => texts(&v, &mut t),
            Err(e) => errors.push(("recall".into(), format!("{k}: {e}"))),
        }
        t
    };
    // The paths the run had first, then the live hook's.
    let mut paths: Vec<(String, String)> = named.iter().map(|k| (format!("recall key={k} (named in the summary or card)"), read(k))).collect();
    let ran = paths.len();
    paths.push(("hook recall on the turn prompt".into(), hook.clone()));
    for k in hook_keys.iter().filter(|k| !named.contains(k)) {
        paths.push((format!("recall key={k} (listed by the hook)"), read(k)));
    }
    // The chain path: hop 1 and hop 2 replies (see `chain_bfs`).
    let mut replies = Vec::new();
    let chain = match &inp.chain {
        Some((dir, cur)) => chain_bfs(&inp.have, *cur, |k| atlas::chain::resolve_parts(dir, k), &mut replies, &mut errors).map_err(|e| format!("{}: {e}", inp.name))?,
        None => Vec::new(),
    };
    let chain_max = chain.iter().map(|x| atlas::chain::est(&x.2)).max().unwrap_or(0);
    // A fact that matches only inside a key token does not match here
    // (`blank_keys`): the summary, card and chain replies are read blanked,
    // and such a row falls through from (a) to (b), then to (c).
    let (have_b, have_k_b, summary_b) = (blank_keys(&inp.have), blank_keys(&inp.have_k), blank_keys(&inp.summary));
    let chain_b: Vec<(u8, &str, String, String)> = chain.iter().map(|(k, h, t, n)| (*h, k.as_str(), blank_keys(t), blank_keys(n))).collect();
    // The chain reply that holds a fact, at the lowest hop: computed once per
    // fact and reading.
    let chain_hit = |f: &str, nd: bool| chain_b.iter().filter(|(_, _, t, n)| (if nd { n } else { t }).contains(f)).map(|(h, k, ..)| (*h, k.to_string())).min();
    let place = |f: &str, have: &str, paths: &[(String, String)], hit: &Option<(u8, String)>| -> (Class, String, Option<u8>) {
        if have.contains(f) {
            (Class::Summary, String::new(), None)
        } else if let Some((via, _)) = paths.iter().find(|(_, t)| t.contains(f)) {
            (Class::Recoverable, via.clone(), None)
        } else if let Some((h, k)) = hit {
            (Class::Recoverable, format!("recall key={k} (chain hop {h})"), Some(*h))
        } else {
            (Class::Lost, String::new(), None)
        }
    };
    let (mut a_inside, mut b_inside) = (0, 0);
    let rows: Vec<Row> = inp
        .facts
        .iter()
        .map(|(f, tool)| {
            let (hit, hit_nd) = (chain_hit(f, false), chain_hit(f, true));
            let (class, via, hop) = place(f, &have_b, &paths[..ran], &hit);
            let (live, live_via, _) = place(f, &have_b, &paths, &hit);
            let (class_nd, ..) = place(f, &have_b, &paths[..ran], &hit_nd);
            let (class_k, ..) = place(f, &have_k_b, &paths[..ran], &hit);
            let (class_k_nd, ..) = place(f, &have_k_b, &paths[..ran], &hit_nd);
            let a_card = class == Class::Summary && !summary_b.contains(f.as_str());
            if inp.have.contains(f.as_str()) && !have_b.contains(f.as_str()) {
                a_inside += 1;
            }
            if hit.is_none() && chain.iter().any(|x| x.2.contains(f.as_str())) {
                b_inside += 1;
            }
            Row { fact: f.clone(), tool: tool.clone(), class, via, live, live_via, hop, class_nd, a_card, class_k, class_k_nd }
        })
        .collect();
    Ok(Split { rows, hook_bytes: hook.len(), hook_keys, named_keys: named, excluded: inp.excluded, chain_max, chain_keys: chain.len(), card_bytes: 0, card_est: 0, replies, errors, a_inside, b_inside })
}

/// The compaction `replay` read at boundary line `b`: replay keeps one
/// compaction per main-chain boundary, in order, by the same test as
/// [`boundary_lines`], so it is the one at `b`'s place among `bounds`,
/// checked by its time.
fn compaction_at<'a>(comps: &'a [atlas::compact::Compaction], bounds: &[usize], lines: &[&str], b: usize) -> Option<&'a atlas::compact::Compaction> {
    if comps.len() != bounds.len() {
        return None;
    }
    let i = bounds.iter().position(|&x| x == b)?;
    let time = atlas::json::parse(lines.get(b)?).ok()?.get("timestamp").and_then(Value::as_str).unwrap_or("").to_string();
    comps.get(i).filter(|c| c.time == time)
}

/// The card a job is scored on: the saved checkpoint card `saved` with the
/// chain card `chain_card` (rebuilt from the pre-cut copy) in it once.
///
/// A checkpoint written before f5d44e1 holds a `tools:` line and no chain
/// card; one written since holds the chain card the hook appended. Either
/// way the saved chain part goes (from its `groups` or `chain_depth` line to
/// the end of its links, an `unread:` line kept) along with any `tools:`
/// line, and the rebuilt copy takes its place, so the run and the smoke are
/// scored alike and every key resolves against the pre-cut copy. The scored
/// card differs from the live one only in the last group key's end offset:
/// the live hook reads the transcript past the scorer's cut.
fn scored_card(saved: &str, chain_card: &str) -> String {
    let mut in_chain = false;
    let old: String = saved
        .split_inclusive('\n')
        .filter(|l| {
            in_chain |= l.starts_with("groups (recall a key for its digest and text):") || l.starts_with("chain_depth: ");
            !l.starts_with("tools: ") && (!in_chain || l.starts_with("unread: "))
        })
        .collect();
    format!("{}\n{chain_card}", old.trim_end())
}

/// Rescores one job: its inputs, then its split on the state as of its cut.
fn rescore_job(o: &Opts, run: &Run, j: &Job) -> Result<Rescored, String> {
    let p = &run.points[j.point];
    let dir = job_dir(o, j);
    let res = read_json(&dir.join("result.json")).ok_or(format!("{}: no result", j.name))?;
    let session = res.get("session").and_then(Value::as_str).unwrap_or("").to_string();
    let cwd = PathBuf::from(res.get("cwd").and_then(Value::as_str).unwrap_or(""));
    let tx = transcript(&cwd, &session)?;
    // The resumed transcript, read once here (and once by `replay`).
    let ttext = std::fs::read_to_string(&tx).map_err(|e| format!("{}: {e}", tx.display()))?;
    let lines: Vec<&str> = ttext.lines().collect();
    let bounds = boundary_lines(&lines);
    let b = bounds.last().copied().filter(|&b| b >= j.keep).ok_or(format!("{}: no boundary of its own", j.name))?;
    let comps = atlas::compact::replay(&tx);
    let c = compaction_at(&comps, &bounds, &lines, b).ok_or(format!("{}: no compaction read at its own boundary (line {b})", j.name))?;
    // The turn prompt, rebuilt from the resumed transcript, must be the
    // recording's: otherwise the hook's recall is not the run's.
    // A point the recording has no prompt after ran no turn: no prompt, so
    // no hook recall (the hook never fires), and the key path alone counts.
    let rec = std::fs::read_to_string(&p.file).map_err(|e| format!("{}: {e}", p.file.display()))?;
    let prompt = next_prompt_in(&ttext, b + 1).map(|(_, t)| t);
    if next_prompt_in(&rec, j.keep).map(|(_, t)| t) != prompt {
        return Err(format!("{}: the resumed turn prompt is not the recording's; the hook's recall cannot be rebuilt exactly", j.name));
    }
    // The pre-cut transcript, as the card's builder and the chain resolver
    // see it: the job's first `keep` lines, under the job's session id
    // (written for the atlas arm, whose card and keys it serves).
    let out = o.work.join("rescore").join(&j.name);
    let pre = out.join("chain");
    std::fs::create_dir_all(&pre).map_err(|e| format!("{}: {e}", pre.display()))?;
    let pre_tx = pre.join(format!("{session}.jsonl"));
    let off = |n: usize| -> usize { lines[..n.min(lines.len())].iter().map(|l| l.len() + 1).sum::<usize>().min(ttext.len()) };
    let (start_off, pre_len) = (off(p.start), off(j.keep));
    let pre_text = &ttext.as_bytes()[..pre_len];
    if j.arm == "atlas" {
        std::fs::write(&pre_tx, pre_text).map_err(|e| format!("{}: {e}", pre_tx.display()))?;
    }
    // The facts every occurrence of which redaction removes are excluded
    // for both arms (0199): what no record's redacted text holds.
    let seg_text: String = atlas::chain::records(&pre_text[start_off..], start_off as u64).iter().map(|r| format!("{}\n", r.text)).collect();
    drop(lines);
    drop(ttext);
    let asof = read_json(&dir.join("as-of.json")).ok_or(format!("{}: no as-of.json", j.name))?;
    let cut = asof.get("cut").and_then(Value::as_str).ok_or(format!("{}: as-of.json names no cut", j.name))?.to_string();
    if super::cut_time(&rec.lines().take(j.keep).collect::<Vec<_>>()).as_deref() != Some(cut.as_str()) {
        return Err(format!("{}: as-of.json's cut is not the recording's cut time", j.name));
    }
    let keep = if j.arm == "atlas" { format!("journal:session:{session}:") } else { String::new() };
    let state = out.join("state");
    let cfg = state_as_of(&dir, &state, &cut, &keep)?;
    // The card, from the pruned store: the arm's newest checkpoint, which
    // must be its own (in its session's lane, written after the cut).
    let (card, head) = if j.arm == "atlas" {
        let (store, _) = open_store(&cfg)?;
        let h = atlas::checkpoint::newest(&store, &session).map_err(|e| e.to_string())?.ok_or(format!("{}: the atlas arm has no checkpoint", j.name))?;
        if !h.key.starts_with(&keep) || h.time.as_str() <= cut.as_str() {
            return Err(format!("{}: the newest checkpoint {} ({}) is not the job's own", j.name, h.key, h.time));
        }
        let (chain_card, head) = atlas::chain::card_and_head(&pre_tx).ok_or(format!("{}: no chain card from the pre-cut copy {}", j.name, pre_tx.display()))?;
        (scored_card(&atlas::checkpoint::card(&h), &chain_card), Some(head))
    } else {
        (String::new(), None)
    };
    let have = format!("{}\n{card}", c.summary);
    let have_k = format!("{}\n{}", c.summary, keys_only(&card));
    let all_facts = atlas::compact::facts_in_by(&rec, p.start, j.keep, FactRule::Named);
    let excluded = all_facts.iter().filter(|(f, _)| !seg_text.contains(f.as_str())).count();
    let facts: Vec<(String, String)> = all_facts.into_iter().filter(|(f, _)| seg_text.contains(f.as_str())).collect();
    let chain = head.map(|h| (pre.clone(), h));
    let inp = Inputs { name: j.name.clone(), session, cwd, have, prompt, tx, summary: c.summary.clone(), have_k, facts, chain, excluded, vectors: o.work.join("rescore").join("vectors.sqlite") };
    let mut split = split(&inp, &cfg)?;
    split.card_bytes = card.len();
    split.card_est = atlas::chain::est(&card);
    // Kept beside the rows, for the token counter (bar 3): the summary of
    // both arms, and the atlas arm's card.
    std::fs::write(out.join("summary.txt"), &c.summary).map_err(|e| format!("{}: {e}", out.display()))?;
    if !card.is_empty() {
        std::fs::write(out.join("card.txt"), &card).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    // The pruned copy has served; only the rows are kept. Every handle on it
    // is closed by now (the hooks, the server and the card's store are
    // dropped, and no embedder was started), so a failure here is an error.
    std::fs::remove_dir_all(&state).map_err(|e| format!("{}: {e}", state.display()))?;
    // So is the pre-cut copy (a whole transcript's size).
    std::fs::remove_dir_all(&pre).map_err(|e| format!("{}: {e}", pre.display()))?;
    let old_facts = atlas::compact::facts_in_by(&rec, p.start, j.keep, FactRule::Serialized);
    let old = (old_facts.iter().filter(|(f, _)| inp.have.contains(f.as_str())).count(), old_facts.len());
    let auth = atlas::compact::facts_in_by(&rec, p.start, j.keep, FactRule::Authored);
    drop(rec);
    let mut by_tool = BTreeMap::new();
    for (_, t) in &auth {
        *by_tool.entry(t.clone()).or_insert(0usize) += 1;
    }
    let authored = (auth.iter().filter(|(f, _)| inp.have.contains(f.as_str())).count(), auth.len(), by_tool);
    let r = Rescored { name: j.name.clone(), arm: j.arm.clone(), at: j.at.clone(), point: j.point, split, old, authored, measured: measure(o, p, j), summary_bytes: c.summary.len(), summary_est: atlas::chain::est(&c.summary) };
    write_job(&out, &r)?;
    Ok(r)
}

/// The job's rows, kept in the work folder (they hold transcript tokens).
fn write_job(out: &Path, r: &Rescored) -> Result<(), String> {
    let s = &r.split;
    let rows: Vec<Value> = s
        .rows
        .iter()
        .map(|x| atlas::json!({"fact": x.fact.clone(), "tool": x.tool.clone(), "class": x.class.name(), "via": x.via.clone(), "live": x.live.name(), "live_via": x.live_via.clone(), "hop": x.hop.map_or(Value::Null, |h| Value::from(h as u64)), "class_no_digest": x.class_nd.name(), "a_from_card": x.a_card, "class_keys_only": x.class_k.name(), "class_keys_only_no_digest": x.class_k_nd.name()}))
        .collect();
    let v = atlas::json!({
        "job": r.name.clone(),
        "rows": Value::Array(rows),
        "hook_bytes": s.hook_bytes as u64,
        "hook_keys": s.hook_keys.clone(),
        "named_keys": s.named_keys.clone(),
        "old_rule": [r.old.0 as u64, r.old.1 as u64],
        "authored": [r.authored.0 as u64, r.authored.1 as u64],
        "excluded": s.excluded as u64,
        "chain_reply_max_est": s.chain_max as u64,
        "chain_keys": s.chain_keys as u64,
        "card_bytes": s.card_bytes as u64,
        "card_est": s.card_est as u64,
        "summary_bytes": r.summary_bytes as u64,
        "summary_est": r.summary_est as u64,
        "chain_replies": Value::Array(s.replies.iter().map(|(k, h, e, cut)| atlas::json!({"key": k.clone(), "hop": *h as u64, "est_uncapped": *e as u64, "cut": *cut})).collect()),
        "errors": Value::Array(s.errors.iter().map(|(k, d)| atlas::json!({"kind": k.clone(), "detail": d.clone()})).collect()),
        "a_inside_key": s.a_inside as u64,
        "b_inside_key": s.b_inside as u64,
    });
    let p = out.join("rescore.json");
    std::fs::write(&p, atlas::json::to_string_pretty(&v)).map_err(|e| format!("{}: {e}", p.display()))
}

fn pct(x: f64) -> String {
    format!("{:.3}", x)
}

/// One job's table, fact by fact (`--job`).
fn print_job(r: &Rescored) {
    let s = &r.split;
    println!("== {} ({} arm, at {})", r.name, r.arm, r.at);
    println!(
        "facts {}: (a) {} (a+b) {} (c) {} on the run's paths; (a+b) {} (c) {} with the live hook",
        s.rows.len(),
        pct(s.rate(|c| c == Class::Summary)),
        pct(s.rate(|c| c != Class::Lost)),
        pct(s.rate(|c| c == Class::Lost)),
        pct(s.live_rate(|c| c != Class::Lost)),
        pct(s.live_rate(|c| c == Class::Lost))
    );
    println!("hook: {} bytes, keys listed: {}; keys named in the summary or card: {}", s.hook_bytes, s.hook_keys.join(", "), s.named_keys.join(", "));
    println!("old rule: {}/{} kept; authored: {}/{} kept, by tool {:?}", r.old.0, r.old.1, r.authored.0, r.authored.1, r.authored.2);
    println!("fact\ttool\trun / live hook\tvia");
    for x in &s.rows {
        let via = if x.via.is_empty() { &x.live_via } else { &x.via };
        println!("{}\t{}\t{} / {}\t{}", atlas::redact::redact(&x.fact), x.tool, x.class.name(), x.live.name(), via);
    }
}

/// The median and the spread (max - min) of some samples.
fn med_spread(v: &[f64]) -> (f64, f64) {
    let mut v: Vec<f64> = v.iter().copied().filter(|x| !x.is_nan()).collect();
    if v.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    v.sort_by(f64::total_cmp);
    let k = v.len();
    let m = if k % 2 == 1 { v[k / 2] } else { (v[k / 2 - 1] + v[k / 2]) / 2.0 };
    (m, v[k - 1] - v[0])
}

/// One arm of one pair on (c): the median over its samples, their spread,
/// and its fact count (the same in every sample: facts come from the cut).
#[derive(Debug, Clone, Copy)]
struct Arm {
    c: f64,
    spread: f64,
    facts: usize,
}

/// How a pair's (c) margin (atlas - plain) sits against its spreads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    /// An arm has no facts: excluded.
    NoData,
    /// |margin| > the larger spread, atlas lower (fewer facts lost).
    Better,
    /// |margin| > the larger spread, atlas higher.
    Worse,
    Within,
}

impl Sep {
    fn name(self) -> &'static str {
        match self {
            Sep::NoData => "no data",
            Sep::Better => "separates, atlas better",
            Sep::Worse => "separates, atlas worse",
            Sep::Within => "within spread",
        }
    }
}

/// A pair separates when |atlas - plain| on (c) exceeds both arms' spreads.
fn separation(atlas: Arm, plain: Arm) -> Sep {
    if atlas.facts == 0 || plain.facts == 0 || atlas.c.is_nan() || plain.c.is_nan() {
        return Sep::NoData;
    }
    let m = atlas.c - plain.c;
    if m.abs() <= atlas.spread.max(plain.spread) {
        Sep::Within
    } else if m < 0.0 {
        Sep::Better
    } else {
        Sep::Worse
    }
}

/// The resolution over a point's pairs: one way only when every pair with
/// data separates that way, MIXED when pairs separate both ways.
fn resolution(seps: &[Sep]) -> &'static str {
    let data: Vec<Sep> = seps.iter().copied().filter(|s| *s != Sep::NoData).collect();
    let (better, worse) = (data.contains(&Sep::Better), data.contains(&Sep::Worse));
    if better && worse {
        "MIXED"
    } else if !data.is_empty() && data.iter().all(|s| *s == Sep::Better) {
        "SEPARATES-ATLAS-BETTER"
    } else if !data.is_empty() && data.iter().all(|s| *s == Sep::Worse) {
        "SEPARATES-ATLAS-WORSE"
    } else {
        "WITHIN SPREAD"
    }
}

/// The rule fixed before the run (brief step 4): atlas wins accuracy when
/// its (c) is no higher than plain's and its (a+b) at least as high. Per
/// fact (a+b) and (c) are complements, so (a+b)+(c) = 1 per job, per median
/// and per mean: the rule reduces to the (c) test (journal 0155 Q3).
fn rule(plain_ab: f64, plain_c: f64, atlas_ab: f64, atlas_c: f64) -> bool {
    atlas_c <= plain_c && atlas_ab >= plain_ab
}

/// A pair's samples of one arm, by `f`: (median, spread).
fn pair_stat(all: &[Rescored], pt: usize, arm: &str, f: &dyn Fn(&Rescored) -> f64) -> (f64, f64) {
    med_spread(&all.iter().filter(|r| r.point == pt && r.arm == arm).map(f).collect::<Vec<_>>())
}

/// An arm at a compaction point, by `f`: the mean over its pairs of the
/// pair's median, and the widest spread.
fn point_stat(all: &[Rescored], points: &BTreeSet<usize>, arm: &str, f: &dyn Fn(&Rescored) -> f64) -> (f64, f64) {
    let (mut sum, mut n, mut spread) = (0.0, 0usize, 0.0f64);
    for &pt in points {
        let (m, s) = pair_stat(all, pt, arm, f);
        if !m.is_nan() {
            sum += m;
            n += 1;
            spread = spread.max(s);
        }
    }
    (if n > 0 { sum / n as f64 } else { f64::NAN }, spread)
}

/// The largest of one arm's samples at a pair, by `f`.
fn pair_max(all: &[Rescored], pt: usize, arm: &str, f: impl Fn(&Rescored) -> usize) -> usize {
    all.iter().filter(|r| r.point == pt && r.arm == arm).map(f).max().unwrap_or(0)
}

/// A pair's label: its first job's name without the arm and sample.
fn pair_label(all: &[Rescored], pt: usize) -> String {
    all.iter().find(|r| r.point == pt).map(|r| r.name.split("-atlas").next().unwrap_or("").split("-plain").next().unwrap_or("").to_string()).unwrap_or_default()
}

/// A per-job statistic.
type Stat<'a> = &'a dyn Fn(&Rescored) -> f64;

/// One pair's bars, each on the atlas arm's median over its samples:
/// 1, (c) at most 2% of the facts, one line per reading in `lost`;
/// 2, (a+b) above plain's by more than the larger spread; 3, the
/// compression in estimated tokens (the bar itself is read in counted
/// tokens on the kept summary.txt and card.txt); 4, (b) only through a key
/// the summary or card prints, which the scorer holds by construction;
/// 5, the largest reply within the cap. No facts is no data, not a pass.
fn print_bars(all: &[Rescored], pt: usize, lost: &[(&str, Stat)], ab: Stat) {
    let label = pair_label(all, pt);
    let facts = pair_max(all, pt, "atlas", |r| r.split.rows.len());
    if facts == 0 {
        println!("  bars at {label}: NO DATA (0 facts)");
        return;
    }
    let pass = |b: bool| if b { "pass" } else { "FAIL" };
    let mut line = format!("  bars at {label} ({facts} facts):");
    for (name, f) in lost {
        let (c, _) = pair_stat(all, pt, "atlas", *f);
        line.push_str(&format!(" 1 {name} (c) {c:.3} {};", pass(c <= 0.02)));
    }
    let (a, sa) = pair_stat(all, pt, "atlas", ab);
    let (p, sp) = pair_stat(all, pt, "plain", ab);
    line.push_str(&format!(" 2 (a+b) {a:.3} vs plain {p:.3}, margin {:+.3} vs spread {:.3} {};", a - p, sa.max(sp), pass(a - p > sa.max(sp))));
    let (ps, _) = pair_stat(all, pt, "plain", &|r: &Rescored| r.summary_est as f64);
    let (at, _) = pair_stat(all, pt, "atlas", &|r: &Rescored| (r.summary_est + r.split.card_est) as f64);
    line.push_str(&format!(" 3 est {ps:.0} / {at:.0} = {:.2}x (counted tokens decide);", ps / at));
    let reply = pair_max(all, pt, "atlas", |r| r.split.replies.iter().map(|x| x.2).max().unwrap_or(0));
    let cut = all.iter().filter(|r| r.point == pt && r.arm == "atlas").map(|r| r.split.replies.iter().filter(|x| x.3).count()).sum::<usize>();
    line.push_str(&format!(" 5 largest reply uncapped {reply} est, [cut] {cut} {}", pass(reply <= atlas::chain::CAP && cut == 0)));
    println!("{line}");
}

/// Per job: the chain replies' largest uncapped estimate and how many the
/// cap cuts, the inside-key counts, and the failed calls by kind (each
/// listed); then the totals.
fn print_instruments(all: &[Rescored]) {
    let (mut max, mut cut, mut a_in, mut b_in, mut errs) = (0, 0, 0, 0, BTreeMap::<String, usize>::new());
    for r in all {
        let s = &r.split;
        let m = s.replies.iter().map(|x| x.2).max().unwrap_or(0);
        let c = s.replies.iter().filter(|x| x.3).count();
        let mut by: BTreeMap<&str, usize> = BTreeMap::new();
        for (k, _) in &s.errors {
            *by.entry(k).or_default() += 1;
            *errs.entry(k.clone()).or_default() += 1;
        }
        println!("{}: {} chain replies, largest uncapped {m} est, [cut] {c}; inside a key: (a) {} (b) {}; errors {by:?}", r.name, s.replies.len(), s.a_inside, s.b_inside);
        for (k, d) in &s.errors {
            println!("  error {k}: {d}");
        }
        (max, cut, a_in, b_in) = (max.max(m), cut + c, a_in + s.a_inside, b_in + s.b_inside);
    }
    println!("instruments: largest uncapped reply {max} est (cap {}), [cut] {cut}; inside a key: (a) {a_in} (b) {b_in}; errors {errs:?}", atlas::chain::CAP);
}

/// Runs the rescore: one job (`--job`, its table) or every job of the run,
/// per compaction point and arm on the medians of the samples.
pub(super) fn run(o: &Opts, run: &Run) -> Result<(), String> {
    if run.smoke {
        return Err("this work folder holds a smoke run (--only); it is never scored".into());
    }
    if let Some(dir) = &o.job {
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let j = run.jobs.iter().find(|j| j.name == name).ok_or(format!("{name}: not a job of this run"))?;
        print_job(&rescore_job(o, run, j)?);
        return Ok(());
    }
    let mut all = Vec::new();
    for j in &run.jobs {
        all.push(rescore_job(o, run, j)?);
    }
    let mut ats: Vec<String> = Vec::new();
    for j in &run.jobs {
        if !ats.contains(&j.at) {
            ats.push(j.at.clone());
        }
    }
    print_instruments(&all);
    let nonempty = all.iter().filter(|r| r.split.hook_bytes > 0).count();
    println!("compact-live rescore: run {} ({} jobs, no model calls); hook recall text non-empty in {nonempty} of {} jobs", run.id, all.len(), all.len());
    println!("per compaction point: the mean over its pairs of each arm's median over samples; spread = the widest max-min over a pair's samples");
    let ab = |r: &Rescored| r.split.rate(|c| c != Class::Lost);
    let lost = |r: &Rescored| r.split.rate(|c| c == Class::Lost);
    let live_ab = |r: &Rescored| r.split.live_rate(|c| c != Class::Lost);
    let live_lost = |r: &Rescored| r.split.live_rate(|c| c == Class::Lost);
    type Col<'a> = (&'static str, &'a dyn Fn(&Rescored) -> f64);
    let lost_nd = |r: &Rescored| r.split.share(|x| x.class_nd, |c| c == Class::Lost);
    let lost_k = |r: &Rescored| r.split.share(|x| x.class_k, |c| c == Class::Lost);
    let lost_k_nd = |r: &Rescored| r.split.share(|x| x.class_k_nd, |c| c == Class::Lost);
    let cols: [Col; 18] = [
        ("(a) in summary or card", &|r: &Rescored| r.split.rate(|c| c == Class::Summary)),
        ("(a) from the card only", &|r: &Rescored| r.split.frac(|x| x.a_card)),
        ("(c) lost, digest not credited", &lost_nd),
        ("(c) lost, card keys only", &lost_k),
        ("(c) lost, card keys only, digest not credited", &lost_k_nd),
        ("summary est tokens", &|r: &Rescored| r.summary_est as f64),
        ("card est tokens", &|r: &Rescored| r.split.card_est as f64),
        ("largest chain reply est tokens", &|r: &Rescored| r.split.chain_max as f64),
        ("(b) at chain hop 1 (facts not already reached by a record key)", &|r: &Rescored| r.split.frac(|x| x.hop == Some(1))),
        ("(b) at chain hop 2 (facts not already reached by a record key)", &|r: &Rescored| r.split.frac(|x| x.hop == Some(2))),
        ("(a+b)", &ab),
        ("(c) lost", &lost),
        ("(a+b) with the live hook", &live_ab),
        ("(c) lost with the live hook", &live_lost),
        ("authored kept", &|r: &Rescored| if r.authored.1 == 0 { f64::NAN } else { r.authored.0 as f64 / r.authored.1 as f64 }),
        ("old-rule fidelity", &|r: &Rescored| if r.old.1 == 0 { 1.0 } else { r.old.0 as f64 / r.old.1 as f64 }),
        ("tokens", &|r: &Rescored| r.measured.as_ref().map_or(f64::NAN, |m| m.tokens)),
        ("summary ratio", &|r: &Rescored| r.measured.as_ref().map_or(f64::NAN, |m| m.ratio)),
    ];
    for at in &ats {
        println!("\ncompaction point {at}:");
        let here: Vec<Rescored> = all.iter().filter(|r| &r.at == at).cloned().collect();
        let points: BTreeSet<usize> = here.iter().map(|r| r.point).collect();
        // The bars (journals 0184-0187, 0198), per pair on the arm's median
        // over its samples, printed first; a pair with no facts is no data.
        for &pt in &points {
            print_bars(&here, pt, &[("as built", &lost), ("digest not credited", &lost_nd), ("card keys only", &lost_k), ("card keys only, digest not credited", &lost_k_nd)], &ab);
        }
        // The rule of record, computed from the values themselves.
        for (label, fab, fc) in [("on the run's paths (verdict of record)", &ab as &dyn Fn(&Rescored) -> f64, &lost as &dyn Fn(&Rescored) -> f64), ("with the live hook", &live_ab, &live_lost)] {
            let (pab, pc, aab, ac) = (point_stat(&here, &points, "plain", fab).0, point_stat(&here, &points, "plain", fc).0, point_stat(&here, &points, "atlas", fab).0, point_stat(&here, &points, "atlas", fc).0);
            let win = rule(pab, pc, aab, ac);
            println!(
                "  rule (fixed before the run) at {at}, {label}: atlas (c) {ac:.3} vs plain {pc:.3}, (a+b) {aab:.3} vs {pab:.3}: atlas {}",
                if win { "WINS accuracy" } else { "does not win accuracy" }
            );
        }
        for (name, f) in &cols {
            let mut line = format!("  {name}:");
            for arm in ["plain", "atlas"] {
                let (mean, spread) = point_stat(&here, &points, arm, *f);
                line.push_str(&format!(" {arm} {mean:.3} (spread {spread:.3});"));
            }
            println!("{}", line.trim_end_matches(';'));
        }
        // What the split moved: facts out of the summary that a hop recovered.
        for arm in ["plain", "atlas"] {
            let mut via: BTreeMap<&str, usize> = BTreeMap::new();
            let (mut b, mut n) = (0, 0);
            for r in here.iter().filter(|r| r.arm == arm) {
                n += r.split.rows.len();
                for x in r.split.rows.iter().filter(|x| x.live == Class::Recoverable) {
                    b += 1;
                    let kind = if x.live_via.starts_with("hook recall") {
                        "hook recall"
                    } else if x.live_via.ends_with("(listed by the hook)") {
                        "key listed by the hook"
                    } else {
                        "key named in summary or card"
                    };
                    *via.entry(kind).or_default() += 1;
                }
            }
            println!("  moved (c) to (b) with the live hook, {arm}: {b} of {n} fact-samples, by path {via:?}");
        }
        // Per pair on the run's paths, then the resolution.
        let mut seps = Vec::new();
        let mut margins = Vec::new();
        for &pt in &points {
            let arm = |a: &str| -> Arm {
                let (c, spread) = pair_stat(&here, pt, a, &lost);
                let facts = here.iter().filter(|r| r.point == pt && r.arm == a).map(|r| r.split.rows.len()).max().unwrap_or(0);
                Arm { c, spread, facts }
            };
            let (a, p) = (arm("atlas"), arm("plain"));
            let sep = separation(a, p);
            let label = pair_label(&here, pt);
            let margin = a.c - p.c;
            println!(
                "  pair {label}: (c) atlas {:.3} (spread {:.3}, {} facts), plain {:.3} (spread {:.3}, {} facts), margin {margin:+.3}: {}",
                a.c,
                a.spread,
                a.facts,
                p.c,
                p.spread,
                p.facts,
                sep.name()
            );
            if sep != Sep::NoData {
                margins.push(margin);
            }
            seps.push(sep);
        }
        let between = if margins.len() < 2 { "n/a (fewer than 2 pairs with data)".to_string() } else { format!("{:.3}", margins.iter().copied().fold(f64::MIN, f64::max) - margins.iter().copied().fold(f64::MAX, f64::min)) };
        println!("  resolution (added after the run) at {at}: {}; between-pair difference of the margins {between}", resolution(&seps));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(class: Class, live: Class) -> Row {
        Row { fact: "f".into(), tool: "Bash".into(), class, via: String::new(), live, live_via: String::new(), hop: None, class_nd: class, a_card: false, class_k: class, class_k_nd: class }
    }

    #[test]
    fn the_headline_rate_ignores_the_live_hook_paths() {
        let s = Split { rows: vec![row(Class::Summary, Class::Summary), row(Class::Lost, Class::Recoverable), row(Class::Lost, Class::Lost), row(Class::Recoverable, Class::Recoverable)], ..Split::default() };
        assert_eq!(s.rate(|c| c == Class::Lost), 0.5);
        assert_eq!(s.live_rate(|c| c == Class::Lost), 0.25);
        assert_eq!(s.rate(|c| c != Class::Lost), 0.5);
        // No facts: everything is in the summary, nothing lost.
        let none = Split::default();
        assert_eq!((none.rate(|c| c == Class::Summary), none.rate(|c| c == Class::Lost)), (1.0, 0.0));
    }

    #[test]
    fn keys_only_keeps_the_keyed_tokens_and_drops_entity_text() {
        let card = "files: src/export.rs\nowed: journal:b-7:0002@a546888bdc23, rules:r1@6b127f44c695\n- turn:s1:b0-b996@3ab4bcdad185 export.rs\nchain_depth: 0";
        assert_eq!(keys_only(card), "journal:b-7:0002@a546888bdc23\nrules:r1@6b127f44c695\nturn:s1:b0-b996@3ab4bcdad185\n");
    }

    /// A checkpoint the hook wrote since f5d44e1 holds the chain card; the
    /// scored card holds the rebuilt one only. An older one (a `tools:` line,
    /// no chain card) gains it, and loses the `tools:` line.
    #[test]
    fn the_scored_card_holds_the_chain_listing_once() {
        let rebuilt = "groups (recall a key for its digest and text):\n- turn:s1:b0-b900@3ab4bcdad185 a.rs\nchain_depth: 2 chain_index: 2\nlinks (recall a key for that segment's card and digest):\n- compact:s1:b0@2e90f19bdd77 2026-10-03T17:32..01:11 a.rs\n";
        let stored = rebuilt.replace("b0-b900@3ab4bcdad185", "b0-b996@512e900cd645");
        let new = format!("atlas checkpoint journal:session:s:0001@b95831fcec1b (the state)\nintent: Next.\n{stored}");
        let old = "atlas checkpoint journal:session:s:0001@b95831fcec1b (the state)\ntools: Bash x3\nintent: Next.\n";
        for saved in [new.as_str(), old] {
            let c = scored_card(saved, rebuilt);
            assert_eq!(c.matches("chain_depth: ").count(), 1, "{c}");
            assert_eq!(c.matches("groups (recall").count(), 1, "{c}");
            assert_eq!(c, format!("atlas checkpoint journal:session:s:0001@b95831fcec1b (the state)\nintent: Next.\n{rebuilt}"));
        }
        let unread = format!("{}unread: none\n", new);
        assert!(scored_card(&unread, rebuilt).contains("unread: none\n"));
    }

    #[test]
    fn median_and_spread_skip_missing_samples() {
        assert_eq!(med_spread(&[0.3, f64::NAN, 0.1, 0.2]), (0.2, 0.19999999999999998));
        assert!(med_spread(&[f64::NAN]).0.is_nan());
    }

    /// A key written with its `@hash` (as a card names its own) is the
    /// record's key; a key-shaped token no record has is not.
    #[test]
    fn keys_named_drops_the_hash_and_admits_store_keys_only() {
        let d = std::env::temp_dir().join(format!("atlas-rescore-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut s = atlas::memory::MemoryStore::open(&d.join("m.sqlite")).unwrap();
        let mut w = s.writer().unwrap();
        for k in ["journal:session:s1:0001", "rule:kept"] {
            w.create(k, "rule", "", "", &atlas::memory::Draft { title: k.into(), body: k.into(), ..Default::default() }, None).unwrap();
        }
        w.commit().unwrap();
        let text = "atlas checkpoint journal:session:s1:0001@6fe82c81cf (recall it), see rule:kept. and rule:gone, journal:session:s1:0001 again";
        assert_eq!(keys_named(text, &s), ["journal:session:s1:0001", "rule:kept"]);
        assert!(exists(&s, "rule:kept") && !exists(&s, "rule:gone"));
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The hop rule (0202): hop 1 is every key the card prints; hop 2 reads
    /// only keys of older sub-segments, so a current group's `next:` key is
    /// not read, an older link's group is, and nothing at depth 3 is. A
    /// dangling key is listed and reads nothing; a missing transcript fails.
    #[test]
    fn the_bfs_reads_older_keys_at_hop_2_only() {
        use atlas::chain::{Miss, Reply};
        let cur = 1000;
        let g = "turn:aaaaaaaa:b1000-b2000@0123456789ab";
        let next = "turn:aaaaaaaa:b1500-b2000@0123456789ac";
        let link = "compact:aaaaaaaa:b900@0123456789ad";
        let old = "turn:aaaaaaaa:b10-b20@0123456789ae";
        let deep = "turn:aaaaaaaa:b30-b40@0123456789af";
        let gone = "turn:aaaaaaaa:b50-b60@0123456789b0";
        let have = format!("summary\n- {g} x.rs\nlinks:\n- {link} 10:00..10:05\n");
        let (mut read, mut replies, mut errors) = (Vec::new(), Vec::new(), Vec::new());
        let chain = chain_bfs(
            &have,
            cur,
            |k| {
                read.push(k.to_string());
                let text = if k == g {
                    format!("{g} 2 records (current)\nnext: {next}\nraw\n")
                } else if k == link {
                    format!("{link} chain_index: 0\n- {old} a.rs\n- {gone}\n")
                } else if k == old {
                    format!("{old} 3 records\nnext: {deep}\nold raw\n")
                } else if k == gone {
                    return Err(Miss::Dangling(format!("{k} is dangling")));
                } else {
                    panic!("{k} is not read")
                };
                Ok(Reply { text, digest: None })
            },
            &mut replies,
            &mut errors,
        )
        .unwrap();
        assert_eq!(read, [g, link, old, gone]);
        assert_eq!(chain.iter().map(|x| (x.0.as_str(), x.1)).collect::<Vec<_>>(), [(g, 1), (link, 1), (old, 2)]);
        assert_eq!(replies.len(), 3);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].1.starts_with(gone));
        // `older`: a link always; a range or part before the current start.
        assert!(atlas::chain::older(link, cur) && atlas::chain::older(old, cur));
        assert!(!atlas::chain::older(next, cur) && !atlas::chain::older(g, cur));
        assert!(atlas::chain::older("turn:aaaaaaaa:b999#2@0123456789ab", cur) && !atlas::chain::older("turn:aaaaaaaa:b1000#2@0123456789ab", cur));
        // A missing transcript fails the job.
        let r = chain_bfs(&have, cur, |k| Err(Miss::NoTranscript(format!("{k}: none"))), &mut Vec::new(), &mut Vec::new());
        assert!(r.is_err());
    }

    /// A key token is blanked where it stands whole; text around it and a
    /// longer token holding it stay.
    #[test]
    fn blank_keys_blanks_whole_key_tokens_only() {
        let t = "see turn:aaaaaaaa:b10-b20@0123456789ab and\n- rule:kept body 0123456789ab\n";
        let b = blank_keys(t);
        assert!(!b.contains("b10-b20") && !b.contains("rule:kept"), "{b}");
        assert!(b.contains("body 0123456789ab"), "{b}");
    }

    fn arm(c: f64, spread: f64, facts: usize) -> Arm {
        Arm { c, spread, facts }
    }

    /// A pair separates only beyond the larger spread; a pair with no facts
    /// in an arm is no data and leaves the resolution to the others.
    #[test]
    fn a_pair_separates_beyond_the_larger_spread() {
        assert_eq!(separation(arm(0.538, 0.058, 52), arm(0.404, 0.058, 52)), Sep::Worse);
        assert_eq!(separation(arm(0.40, 0.05, 10), arm(0.50, 0.02, 10)), Sep::Better);
        assert_eq!(separation(arm(0.899, 0.0, 9), arm(0.910, 0.05, 9)), Sep::Within);
        assert_eq!(separation(arm(0.5, 0.2, 10), arm(0.3, 0.2, 10)), Sep::Within, "a margin equal to the spread does not separate");
        assert_eq!(separation(arm(1.0, 0.0, 0), arm(0.0, 0.0, 10)), Sep::NoData);
        assert_eq!(resolution(&[Sep::Worse, Sep::NoData]), "SEPARATES-ATLAS-WORSE");
        assert_eq!(resolution(&[Sep::Better, Sep::Better]), "SEPARATES-ATLAS-BETTER");
        assert_eq!(resolution(&[Sep::Better, Sep::Worse]), "MIXED");
        assert_eq!(resolution(&[Sep::Worse, Sep::Within]), "WITHIN SPREAD");
        assert_eq!(resolution(&[Sep::NoData]), "WITHIN SPREAD");
        assert!(rule(0.4, 0.6, 0.4, 0.6) && !rule(0.4, 0.6, 0.39, 0.61));
    }
}
