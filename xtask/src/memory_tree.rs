//! `cargo xtask memory-tree-check --config CONFIG [--dir DIR]`: the memory
//! index tree's navigation and stability check, without writing.
//!
//! * Navigation (`atlas::memory::tree::check`): every memory file is reached
//!   from the root in at most 3 reads, pinned memories are in the root, no
//!   memory is listed by two hubs (a pinned one also by the root), no
//!   generated hub is orphaned, no link dangles, no hub is over `hub_max`.
//! * Stability: a rebuild of what is on disk would write nothing.
//!
//! DIR defaults to the root of the source the tree's export syncs from.

use std::path::PathBuf;

/// What the check found, for the eval's report too.
pub struct Outcome {
    /// Problems, one line each (empty: the tree passes).
    pub problems: Vec<String>,
    /// Reads to each memory reached, by stem.
    pub depth: std::collections::BTreeMap<String, usize>,
}

/// Checks the tree in `dir` (or the configured memory source) and prints a
/// summary.
pub fn check(c: &atlas::config::Config, dir: Option<PathBuf>) -> Result<Outcome, String> {
    use atlas::memory::tree;
    let db = c.db.clone().ok_or("the configuration names no db")?;
    let m = atlas::memory::settings::Settings::from_config(Some(c), &db).map_err(|e| e.to_string())?;
    let spec = m.tree.as_ref().ok_or("memory.tree is not configured")?;
    let export = m.exports.iter().find(|e| e.name == spec.export).ok_or_else(|| format!("no memory export named {:?}", spec.export))?;
    let dir = match dir {
        Some(d) => d,
        None => c.sources.iter().find(|s| s.name == spec.export).map(|s| s.root.clone()).ok_or_else(|| format!("no source named {:?}; pass --dir", spec.export))?,
    };
    let nav = tree::check(&dir, &export.index, spec).map_err(|e| e.to_string())?;
    let mut embed = |t: &[String]| tree::lexical_vectors(t);
    let (churn, _) = tree::rebuild(&dir, &export.index, spec, &mut embed, false).map_err(|e| e.to_string())?;
    let bpt = m.bytes_per_token;
    println!(
        "memory tree {}: {} memories, {} hubs; reads 1/2/3/more {}/{}/{}/{}; widest hub {}; root {} lines, {} bytes (~{:.0} tokens); root plus path mean ~{:.0} tokens, max ~{:.0}; a rebuild would write {} and delete {} files",
        dir.display(),
        nav.leaves,
        nav.hubs,
        nav.reads[1],
        nav.reads[2],
        nav.reads[3],
        nav.reads[4],
        nav.fanout_max,
        nav.root_lines,
        nav.root_bytes,
        nav.root_bytes as f64 / bpt,
        nav.path_bytes_mean / bpt,
        nav.path_bytes_max as f64 / bpt,
        churn.written.len(),
        churn.deleted.len()
    );
    let mut problems = nav.problems.clone();
    // Placing a new memory may need the model; only a rebuild of what is
    // already placed is held to zero here.
    if churn.added.is_empty() && churn.changed() {
        problems.push(format!("a rebuild with nothing new would write {:?} and delete {:?}", churn.written, churn.deleted));
    }
    for p in &problems {
        println!("    tree: {p}");
    }
    Ok(Outcome { problems, depth: nav.depth })
}
