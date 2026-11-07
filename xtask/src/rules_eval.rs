//! `cargo xtask rules-eval`: which ruling holds now.
//!
//! The memory eval's questions ask for facts, and it passes while a replaced
//! ruling ranks above the one that holds. This asks one thing of a frozen set of
//! pairs: is the live ruling first? Run it once per arm, each arm a configuration
//! with its own copy of the store (today: unset, no marks; live: `memory.rules`
//! on, the answers applied), so nothing in here reads a bar. The no-loss guard
//! is `memory-eval` on the same configuration.
//!
//! Each question goes two ways, by query recall and by the prompt hook, each with
//! a fresh session. A question's cost is the first answer plus one follow-up, the
//! live ruling's record when the answer lists it and the facts are not already
//! there: the true cost, which better follow-through raises. `fact:` lines also
//! report whether the pointer line alone carries the answer.
//!
//! The set is plain text, kept outside the repository, by path:
//!
//! ```text
//! H001
//! ask: Does the widget cache have a bound?
//! query: widget cache bound
//! live: rules:r00003
//! old: rules:r00001
//! fact: grows freely
//! ```
//!
//! Rows carry question ids and counts only. `--dump DIR` writes what was sent, one
//! file per question and way, for the real counter; never into the repository.

use std::path::{Path, PathBuf};

use atlas::json::Value;

use crate::memory_eval::{holds, keys_in, norm, server};

/// One pair of the set.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pair {
    /// `H001`.
    pub id: String,
    /// The prompt the hook replay sends.
    pub ask: String,
    /// The query recall's query.
    pub query: String,
    /// The ruling that holds now.
    pub live: String,
    /// The ruling it replaces or amends.
    pub old: String,
    /// What the live ruling says; each is a set of accepted spellings.
    pub facts: Vec<Vec<String>>,
}

/// Parses a set.
///
/// # Errors
///
/// A message naming the line, for a field outside a block, an unknown field, a
/// repeated id, or a block missing `ask`, `query`, `live` or `old`.
pub fn parse(text: &str) -> Result<Vec<Pair>, String> {
    let mut out: Vec<Pair> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let Some((field, value)) = l.split_once(": ") else {
            if out.iter().any(|p| p.id == l) {
                return Err(format!("line {}: {l} is used twice", i + 1));
            }
            out.push(Pair { id: l.to_string(), ..Pair::default() });
            continue;
        };
        let p = out.last_mut().ok_or_else(|| format!("line {}: a field outside a block", i + 1))?;
        match field {
            "ask" => p.ask = value.to_string(),
            "query" => p.query = value.to_string(),
            "live" => p.live = value.to_string(),
            "old" => p.old = value.to_string(),
            "fact" => p.facts.push(value.split(" | ").map(norm).collect()),
            f => return Err(format!("line {}: unknown field {f}", i + 1)),
        }
    }
    for p in &out {
        if p.ask.is_empty() || p.query.is_empty() || p.live.is_empty() || p.old.is_empty() {
            return Err(format!("{}: needs ask, query, live and old", p.id));
        }
    }
    Ok(out)
}

/// What one way of asking returned.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Way {
    /// The live ruling is the first key listed.
    pub first: bool,
    /// The replaced ruling is listed ahead of the live one.
    pub old_ahead: bool,
    /// The live ruling is listed at all.
    pub listed: bool,
    /// The replaced or amended ruling is listed at all: a partial's old one still
    /// holds in part, so losing it loses a live ruling.
    pub old_listed: bool,
    /// Every fact is in the answer alone (true with no facts given).
    pub alone: bool,
}

/// Reads one answer against a pair.
pub fn read(p: &Pair, answer: &str) -> Way {
    let keys = keys_in(answer);
    let at = |k: &str| keys.iter().position(|x| x == k);
    let (live, old) = (at(&p.live), at(&p.old));
    Way {
        first: live == Some(0),
        old_ahead: match (old, live) {
            (Some(o), Some(l)) => o < l,
            (Some(_), None) => true,
            _ => false,
        },
        listed: live.is_some(),
        old_listed: old.is_some(),
        alone: p.facts.iter().all(|f| holds(answer, f)),
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    match v.len() {
        0 => 0.0,
        n if n % 2 == 1 => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}

#[derive(Default)]
struct Tally {
    first: usize,
    old_ahead: usize,
    listed: usize,
    old_listed: usize,
    alone: usize,
    with_facts: usize,
    tokens: Vec<f64>,
}

impl Tally {
    fn add(&mut self, w: &Way, has_facts: bool, tokens: f64) {
        self.first += w.first as usize;
        self.old_ahead += w.old_ahead as usize;
        self.listed += w.listed as usize;
        self.old_listed += w.old_listed as usize;
        // With no facts `alone` is true of every pair and says nothing.
        self.alone += (has_facts && w.alone) as usize;
        self.with_facts += has_facts as usize;
        self.tokens.push(tokens);
    }
    fn line(&self, way: &str, n: usize) -> String {
        let total: f64 = self.tokens.iter().sum();
        let alone = if self.with_facts > 0 { format!(", facts in the first answer alone {}/{}", self.alone, self.with_facts) } else { String::new() };
        format!(
            "{way}: live first {}/{n}, listed {}/{n}, old listed {}/{n}, old ahead {}/{n}{alone}, tokens by bytes median {:.0} total {total:.0}",
            self.first,
            self.listed,
            self.old_listed,
            self.old_ahead,
            median(self.tokens.clone())
        )
    }
}

/// Options of one run.
pub struct Opts {
    /// The arm's configuration (its db and store are the arm's copies).
    pub config: PathBuf,
    /// The frozen set.
    pub holds: PathBuf,
    /// The project to run as.
    pub project: Option<String>,
    /// The folder the replayed session works in.
    pub cwd: Option<PathBuf>,
    /// One row per question.
    pub rows: bool,
    /// Where to write what was sent.
    pub dump: Option<PathBuf>,
}

fn dump(dir: Option<&Path>, id: &str, way: &str, text: &str) -> Result<(), String> {
    let Some(d) = dir else { return Ok(()) };
    std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    std::fs::write(d.join(format!("{id}.{way}.txt")), text).map_err(|e| e.to_string())
}

/// Runs one arm and prints its report.
///
/// # Errors
///
/// A message for an unreadable or invalid set or configuration, or a pair whose
/// keys the store does not hold: such a pair tests nothing.
pub fn run(o: &Opts) -> Result<(), String> {
    let bytes = std::fs::read(&o.holds).map_err(|e| format!("{}: {e}", o.holds.display()))?;
    let pairs = parse(&String::from_utf8_lossy(&bytes))?;
    let raw = atlas::config::Config::load(&o.config).map_err(|e| e.to_string())?;
    let cwd = o.cwd.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let c = atlas::project::resolve(&raw, &cwd, o.project.as_deref()).map_err(|e| e.to_string())?.config;
    let db = c.db.clone().ok_or("the configuration names no db")?;
    let settings = atlas::memory::settings::Settings::from_config(Some(&c), &db).map_err(|e| e.to_string())?;
    let bpt = c.bytes_per_token.unwrap_or(atlas::tokens::DEFAULT_BYTES_PER_TOKEN);
    {
        let mem = atlas::memory::MemoryStore::open(&settings.store).map_err(|e| e.to_string())?;
        for p in &pairs {
            for k in [&p.live, &p.old] {
                if mem.head(k).map_err(|e| e.to_string())?.is_none() {
                    return Err(format!("{}: the store holds no {k}", p.id));
                }
            }
        }
    }
    println!("rules-eval set sha256 {} ({} pairs), memory.rules {:?}", atlas::sha256::hex(&bytes), pairs.len(), settings.recall.rules);
    // As memory-eval does: a background embedder must not decide a hook score.
    if let Some(n) = crate::memory_eval::embed_all_pending(&settings)? {
        println!("pre-embed: {n} pending record vectors");
    }
    let mut srv = server(&c)?;
    let mut hooks = atlas::hooks::Hooks::open(settings).map_err(|e| e.to_string())?;
    let (mut by_query, mut by_hook) = (Tally::default(), Tally::default());
    for p in &pairs {
        let s = ask_pair(&mut srv, &mut hooks, p, &cwd);
        dump(o.dump.as_deref(), &p.id, "hook", &s.hook)?;
        by_hook.add(&s.hook_way, !p.facts.is_empty(), s.hook.len() as f64 / bpt);
        dump(o.dump.as_deref(), &p.id, "query", &s.query)?;
        by_query.add(&s.query_way, !p.facts.is_empty(), s.query.len() as f64 / bpt);
        if o.rows {
            println!("{} query {} bytes={}; hook {} bytes={}", p.id, row(p, &s.query_way), s.query.len(), row(p, &s.hook_way), s.hook.len());
        }
    }
    println!("{}", by_query.line("query", pairs.len()));
    println!("{}", by_hook.line("hook", pairs.len()));
    Ok(())
}

/// One pair's two ways, each with its follow-up.
struct Scored {
    hook_way: Way,
    /// The hook's context plus the follow-up.
    hook: String,
    query_way: Way,
    /// The query's answer plus the follow-up.
    query: String,
}

/// Asks one pair. Every recall says `fresh`: the server compares repeats per
/// caller, so an identical call would come back as a reference of a few bytes,
/// and the arm that lists a ruling more often would be undercounted more.
fn ask_pair(srv: &mut atlas::tools::Server, hooks: &mut atlas::hooks::Hooks, p: &Pair, cwd: &Path) -> Scored {
    let mut ask = |args: &Value| srv.answer("recall", args).map(|x| x.into_text()).unwrap_or_else(|e| format!("error: {e}"));
    // One follow-up, when the answer lists the ruling and does not settle it.
    let settled = |w: &Way| !w.listed || (w.alone && !p.facts.is_empty());
    let live_args = atlas::json!({"key": p.live.clone(), "fresh": true});
    let q = hook_answer(hooks, p, cwd);
    let hook_way = read(p, &q);
    let hook_more = if settled(&hook_way) { String::new() } else { ask(&live_args) };
    let a = ask(&atlas::json!({"query": p.query.clone(), "fresh": true}));
    let query_way = read(p, &a);
    let query_more = if settled(&query_way) { String::new() } else { ask(&live_args) };
    Scored { hook_way, hook: format!("{q}{hook_more}"), query_way, query: format!("{a}{query_more}") }
}

fn row(p: &Pair, w: &Way) -> String {
    let alone = if p.facts.is_empty() { String::new() } else { format!(" alone={}", w.alone as u8) };
    format!("first={} listed={} old_listed={} old_ahead={}{alone}", w.first as u8, w.listed as u8, w.old_listed as u8, w.old_ahead as u8)
}

fn hook_answer(hooks: &mut atlas::hooks::Hooks, p: &Pair, cwd: &Path) -> String {
    let ev = atlas::hooks::HookEvent::from_value(&atlas::json!({
        "event": "UserPromptSubmit",
        "session_id": format!("rules-eval-{}", p.id),
        "prompt": p.ask.clone(),
        "cwd": cwd.to_string_lossy().into_owned()
    }));
    hooks.handle(&ev).context
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = "# invented\nH001\nask: Does the widget cache have a bound?\nquery: widget cache bound\nlive: rules:r00003\nold: rules:r00001\nfact: grows freely | no bound\n";

    #[test]
    fn a_set_parses_and_a_short_block_does_not() {
        let p = &parse(SET).unwrap()[0];
        assert_eq!((p.id.as_str(), p.live.as_str(), p.old.as_str(), p.facts.len()), ("H001", "rules:r00003", "rules:r00001", 1));
        assert_eq!(p.facts[0].len(), 2);
        assert!(parse("H001\nask: a\n").unwrap_err().contains("needs"));
        assert!(parse("ask: a\n").unwrap_err().contains("outside"));
        assert!(parse(&format!("{SET}H001\n")).unwrap_err().contains("twice"));
        assert!(parse("H001\nwho: a\n").unwrap_err().contains("unknown"));
    }

    #[test]
    fn an_answer_is_read_for_the_live_rulings_place() {
        let p = &parse(SET).unwrap()[0];
        let live_first = read(p, "- rules:r00003: 01-04 it grows freely [replaces rules:r00001]\n- rules:r00009: other\n");
        assert_eq!(live_first, Way { first: true, old_ahead: false, listed: true, old_listed: false, alone: true });
        let old_first = read(p, "- rules:r00001: the bound\n- rules:r00003: later\n");
        assert_eq!(old_first, Way { first: false, old_ahead: true, listed: true, old_listed: true, alone: false });
        // The old one alone is ahead of a live one that never shows.
        let lost = read(p, "- rules:r00001: the bound\n");
        assert_eq!(lost, Way { first: false, old_ahead: true, listed: false, old_listed: true, alone: false });
        assert_eq!(read(p, ""), Way::default());
    }

    /// A repeat of a pair must cost what the first asking cost: without `fresh`
    /// its follow-up comes back as a reference, and nothing says so.
    #[test]
    fn the_same_pair_twice_in_one_server_costs_the_same() {
        let d = std::env::temp_dir().join(format!("atlas-rules-eval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("vault")).unwrap();
        let path = d.join("c.json");
        let root = d.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &path,
            format!(r#"{{"db":"{root}/index.sqlite","vaults":[{{"name":"notes","path":"{root}/vault"}}],"watch":false,"memory":{{"store":"{root}/memory.sqlite"}}}}"#),
        )
        .unwrap();
        let raw = atlas::config::Config::load(&path).unwrap();
        let c = atlas::project::resolve(&raw, &d, None).unwrap().config;
        let settings = atlas::memory::settings::Settings::from_config(Some(&c), c.db.as_ref().unwrap()).unwrap();
        {
            let mut mem = atlas::memory::MemoryStore::open(&settings.store).unwrap();
            let mut w = mem.writer().unwrap();
            let rows = [("rule:one", "the widget cache bound is sixty four entries"), ("rule:two", "exports are written through a rename"), ("rule:three", "the queue drains oldest first"), ("rule:four", "lanes hand off by journal"), ("rule:five", "shaders are checked at build time")];
            for (key, title) in rows {
                let draft = atlas::memory::Draft { title: title.into(), body: "body of the ruling, repeated at length so a follow-up costs bytes".into(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
                w.create(key, "rule", "", "project", &draft, None).unwrap();
            }
            w.commit().unwrap();
        }
        let mut srv = server(&c).unwrap();
        let mut hooks = atlas::hooks::Hooks::open(settings).unwrap();
        let pair = |id: &str| Pair { id: id.into(), ask: "widget cache bound entries".into(), query: "widget cache bound entries".into(), live: "rule:one".into(), old: "rule:two".into(), facts: Vec::new() };
        let first = ask_pair(&mut srv, &mut hooks, &pair("A"), &d);
        let again = ask_pair(&mut srv, &mut hooks, &pair("B"), &d);
        assert!(first.query_way.listed, "{}", first.query);
        assert_eq!(first.query, again.query);
        assert_eq!(first.hook, again.hook);
        assert!(first.query.contains("body of the ruling"), "the follow-up is the record: {}", first.query);
        drop((srv, hooks));
        let _ = std::fs::remove_dir_all(&d);
    }
}
