//! `cargo xtask hook-bench [--records N] [--prompts N] [--model PATH]
//! [--threads N]`: the prompt hook's cost on a resident hook state, on a
//! temp store of N recallable records (default 1,604), each arm timed
//! in-process around [`Hooks::warm`] or [`Hooks::handle`]:
//!
//! * **part: ...**: the parts of the warm-up alone, each on a fresh store
//!   connection: the term counts and lanes, [`Model::load`], the pending
//!   check ([`vectors::pending`]) and a cold [`RecVecs::load`].
//! * **warm-up**: [`Hooks::warm`] on a fresh hook state (what a server
//!   start runs in the background).
//! * **first prompt, no warm-up**: a fresh state's first prompt (it loads
//!   the model and the vectors itself).
//! * **first prompt after warm-up**.
//! * **warm prompt**: a later prompt with no store commit between.
//! * **prompt after a journal write**: another connection commits a
//!   journal entry before each prompt (not timed).
//!
//! Each arm prints its median and p95 in ms and the record vectors read
//! ([`Hooks::vectors_read`]) per run. The model is the test encoder
//! ([`atlas::embed::tiny_model_bytes`], 16 dimensions) unless `--model`
//! names an existing `.atm` file, which is only read. `--threads` is the
//! background embedder's (the query is encoded on one thread).
//!
//! Nothing is read or written outside a scratch folder under the system
//! temp folder, removed at the end; no configuration file is read.

use std::path::{Path, PathBuf};
use std::time::Instant;

use atlas::embed::Model;
use atlas::hooks::{HookEvent, Hooks};
use atlas::memory::settings::Settings;
use atlas::memory::vectors::{self, RecVecs};
use atlas::memory::{Draft, MemoryStore};

pub const USAGE: &str = "usage: cargo xtask hook-bench [--records N] [--prompts N] [--model PATH.atm] [--threads N]";

/// Words the records and prompts are drawn from.
const WORDS: &[&str] = &[
    "widget", "cache", "eviction", "sprocket", "gadget", "exporter", "garden", "schedule", "queue", "worker", "thread", "lock", "store",
    "index", "query", "budget", "token", "vector", "model", "record", "journal", "lane", "review", "build", "test", "release", "branch",
    "merge", "commit", "hook", "prompt", "session", "agent", "brief", "server", "client", "socket", "timeout", "retry", "backoff", "batch",
    "stream", "buffer", "page", "file", "folder", "path", "config", "default", "setting", "flag", "error", "warning", "limit", "floor",
    "ceiling", "score", "rank", "match", "filter", "sort", "order", "shape", "layout", "render", "frame", "column", "table", "parser",
    "unit", "schema", "map", "card", "report", "metric", "cost", "speed", "memory", "disk", "network", "packet", "header", "body", "title",
    "reason",
];

struct Opts {
    records: usize,
    prompts: usize,
    model: Option<PathBuf>,
    threads: usize,
}

fn parse(args: Vec<String>) -> Result<Opts, String> {
    let mut o = Opts { records: 1604, prompts: 50, model: None, threads: 2 };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value\n{USAGE}"));
        let num = |v: String| v.parse::<usize>().map_err(|_| format!("{a}: {v:?} is not a count"));
        match a.as_str() {
            "--records" => o.records = num(val()?)?,
            "--prompts" => o.prompts = num(val()?)?.max(1),
            "--threads" => o.threads = num(val()?)?.max(1),
            "--model" => o.model = Some(PathBuf::from(val()?)),
            _ => return Err(USAGE.into()),
        }
    }
    if let Some(m) = &o.model {
        check_model(m)?;
    }
    Ok(o)
}

/// An existing `.atm` file outside any atlas home: the bench never reads a
/// live atlas folder.
fn check_model(m: &Path) -> Result<(), String> {
    if m.extension().and_then(|e| e.to_str()) != Some("atm") || !m.is_file() {
        return Err(format!("--model {}: not an existing .atm file", m.display()));
    }
    let full = std::fs::canonicalize(m).map_err(|e| format!("--model {}: {e}", m.display()))?;
    let homes: Vec<PathBuf> =
        std::env::var_os("ATLAS_HOME").map(PathBuf::from).into_iter().filter_map(|h| std::fs::canonicalize(h).ok()).collect();
    if full.components().any(|c| c.as_os_str() == ".atlas") || homes.iter().any(|h| full.starts_with(h)) {
        return Err(format!("--model {}: inside an atlas home; copy the file elsewhere", m.display()));
    }
    Ok(())
}

/// A deterministic word sequence (xorshift), so every run seeds the same store.
struct Gen(u64);

impl Gen {
    fn words(&mut self, n: usize) -> String {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            out.push(WORDS[(self.0 % WORDS.len() as u64) as usize]);
        }
        out.join(" ")
    }
}

fn draft(title: String, body: String) -> Draft {
    Draft { title, body, reason: "hook bench".into(), author: "bench".into(), client: "bench".into(), ..Draft::default() }
}

/// Median and p95 of `ms`, in ms.
fn stats(ms: &mut [f64]) -> (f64, f64) {
    ms.sort_by(f64::total_cmp);
    let p = |q: f64| ms[((q * ms.len() as f64).ceil() as usize).clamp(1, ms.len()) - 1];
    (ms[ms.len() / 2], p(0.95))
}

struct Report {
    rows: Vec<(String, usize, f64, f64, f64)>,
}

impl Report {
    /// One arm: its runs' ms and the vectors read over all of them.
    fn arm(&mut self, name: &str, mut ms: Vec<f64>, read: usize) {
        let n = ms.len();
        let (med, p95) = stats(&mut ms);
        self.rows.push((name.to_string(), n, med, p95, read as f64 / n as f64));
    }
}

/// Runs the bench and prints its report.
pub fn run(args: Vec<String>) -> Result<(), String> {
    let o = parse(args)?;
    let dir = std::env::temp_dir().join(format!("atlas-hook-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let r = bench(&o, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn bench(o: &Opts, dir: &Path) -> Result<(), String> {
    let e = |e: atlas::Error| e.to_string();
    let mut s = Settings::from_config(None, &dir.join("state").join("index.sqlite")).map_err(e)?;
    s.snapshot_secs = 0;
    s.embed_threads = o.threads;
    match &o.model {
        Some(m) => s.model = m.clone(),
        None => {
            std::fs::create_dir_all(s.model.parent().unwrap_or(dir)).map_err(|e| e.to_string())?;
            std::fs::write(&s.model, atlas::embed::tiny_model_bytes(true, 7)).map_err(|e| e.to_string())?;
        }
    }
    let model = Model::load(&s.model).map_err(e)?;

    // The store: N recallable records, every one embedded.
    let mut g = Gen(0x9e37_79b9_7f4a_7c15);
    let mut store = MemoryStore::open(&s.store).map_err(e)?;
    {
        let mut w = store.writer().map_err(e)?;
        for n in 0..o.records {
            let (title, body) = (g.words(4), format!("- {}.\n", g.words(30)));
            w.create(&format!("bench:r{n:05}"), "rule", "bench", "", &draft(title, body), None).map_err(e)?;
        }
        w.commit().map_err(e)?;
    }
    while vectors::embed_pending(&store, &model, 128, o.threads).map_err(e)? > 0 {}
    let reps = o.prompts.clamp(1, 20);
    let mut rep = Report { rows: Vec::new() };
    let mut journal = 0;
    let mut write_journal = |store: &mut MemoryStore, g: &mut Gen| -> Result<(), String> {
        journal += 1;
        let mut w = store.writer().map_err(e)?;
        w.create(&format!("journal:bench:{journal:04}"), "journal", "bench", "note", &draft("progress".into(), g.words(20)), None)
            .map_err(e)?;
        w.commit().map_err(e)
    };
    let prompt = |h: &mut Hooks, g: &mut Gen| -> (f64, usize) {
        let ev = HookEvent {
            event: "UserPromptSubmit".into(),
            session_id: "bench".into(),
            prompt: format!("what about the {}?", g.words(8)),
            ..HookEvent::default()
        };
        let before = h.vectors_read();
        let t = Instant::now();
        let _ = h.handle(&ev);
        (t.elapsed().as_secs_f64() * 1e3, h.vectors_read() - before)
    };

    // The parts of the warm-up alone, each on a fresh connection as a fresh
    // hook state's is: the term frequencies and lanes, the model, the
    // pending check and the vectors.
    let mut part = |name: &str, f: &dyn Fn(&MemoryStore) -> Result<usize, String>| -> Result<(), String> {
        let (mut ms, mut read) = (Vec::new(), 0);
        for _ in 0..reps {
            let fresh = MemoryStore::open(&s.store).map_err(e)?;
            let t = Instant::now();
            read += f(&fresh)?;
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        rep.arm(name, ms, read);
        Ok(())
    };
    part("part: term counts and lanes", &|st| {
        st.rarest(&[], 0).map_err(e)?;
        st.lanes().map_err(e)?;
        Ok(0)
    })?;
    part("part: model load", &|_| Model::load(&s.model).map(|_| 0).map_err(e))?;
    part("part: pending check", &|st| vectors::pending(st, &model.id, 1).map(|_| 0).map_err(e))?;
    part("part: vector load", &|st| RecVecs::load(st, &model.id, model.dim).map(|v| v.read).map_err(e))?;

    let open = || Hooks::open(s.clone()).map_err(e);
    let (mut ms, mut read) = (Vec::new(), 0);
    for _ in 0..reps {
        let mut h = open()?;
        let t = Instant::now();
        h.warm();
        ms.push(t.elapsed().as_secs_f64() * 1e3);
        read += h.vectors_read();
    }
    rep.arm("warm-up", ms, read);

    let (mut ms, mut read) = (Vec::new(), 0);
    for _ in 0..reps {
        let (t, r) = prompt(&mut open()?, &mut g);
        ms.push(t);
        read += r;
    }
    rep.arm("first prompt, no warm-up", ms, read);

    let (mut ms, mut read) = (Vec::new(), 0);
    for _ in 0..reps {
        let mut h = open()?;
        h.warm();
        let (t, r) = prompt(&mut h, &mut g);
        ms.push(t);
        read += r;
    }
    rep.arm("first prompt after warm-up", ms, read);

    let mut h = open()?;
    h.warm();
    let _ = prompt(&mut h, &mut g);
    let (mut ms, mut read) = (Vec::new(), 0);
    for _ in 0..o.prompts {
        let (t, r) = prompt(&mut h, &mut g);
        ms.push(t);
        read += r;
    }
    rep.arm("warm prompt", ms, read);

    let mut other = MemoryStore::open(&s.store).map_err(e)?;
    let (mut ms, mut read) = (Vec::new(), 0);
    for _ in 0..o.prompts {
        write_journal(&mut other, &mut g)?;
        let (t, r) = prompt(&mut h, &mut g);
        ms.push(t);
        read += r;
    }
    rep.arm("prompt after a journal write", ms, read);
    drop((h, other, store));

    println!(
        "hook-bench: {} recallable records, model {} (dim {}), embed threads {}, query encode on 1 thread, {} prompts per prompt arm, {reps} runs per cold arm",
        o.records, model.id, model.dim, o.threads, o.prompts
    );
    println!("{:<30} {:>5} {:>10} {:>10} {:>14}", "arm", "runs", "median ms", "p95 ms", "vectors/run");
    for (name, n, med, p95, read) in &rep.rows {
        println!("{name:<30} {n:>5} {med:>10.2} {p95:>10.2} {read:>14.1}");
    }
    Ok(())
}
