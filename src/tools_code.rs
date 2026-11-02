//! The `symbol` tool over the code index (see [`crate::code`]). A child
//! module of `tools`, so it shares the server's private state.

use crate::json::{Map, Value};
use crate::json;

use super::{arg_str, opt_uint, Answer, Fmt, Server};
use crate::code::{self, Limits, SymbolHit};
use crate::code_index::RootState;
use crate::error::{Error, Result};

/// Definitions a `symbol` answer lists by default.
pub const DEFAULT_DEFS: usize = 10;
/// References a `symbol` answer lists by default.
pub const DEFAULT_REFS: usize = 10;
/// Impl edges a `symbol` answer lists by default.
pub const DEFAULT_IMPLS: usize = 10;
/// The most rows of any section.
pub const MAX_ROWS: usize = 500;

fn hit_json(h: &SymbolHit) -> Value {
    let mut v = json!({"root": h.root, "path": h.path, "line": h.line, "kind": h.kind, "container": h.container, "within": h.within, "text": h.text.trim()});
    if h.end_line > h.line {
        v["end_line"] = json!(h.end_line);
    }
    v
}

impl Server {
    /// One line per code root that is not fresh: pending, running, failed,
    /// never indexed, or indexed without rust-analyzer.
    fn code_freshness(&self) -> Result<Vec<String>> {
        let Some(c) = &self.code else { return Ok(Vec::new()) };
        let records = code::root_records(self.store.conn())?;
        let status = c.indexer.as_ref().map(|i| i.status()).unwrap_or_default();
        let mut out = Vec::new();
        for r in &c.setup.roots {
            match status.get(&r.name) {
                Some(RootState::Pending) => out.push(format!("stale {}: changed, reindex pending", r.name)),
                Some(RootState::Running) => out.push(format!("stale {}: reindex running", r.name)),
                Some(RootState::Failed(e)) => out.push(format!("stale {}: last index failed: {e}", r.name)),
                Some(RootState::Deferred(why)) => out.push(format!("stale {}: reindex waiting: {why}", r.name)),
                _ => {}
            }
            match records.get(&r.name) {
                None => out.push(format!("{}: not indexed yet", r.name)),
                Some(x) if x.rust.starts_with("scan only") => out.push(format!("{}: rust definitions only ({})", r.name, x.rust)),
                _ => {}
            }
        }
        Ok(out)
    }

    pub(super) fn symbol(&mut self, a: &Map, f: Fmt) -> Result<Answer> {
        if self.code.is_none() {
            return Err(Error::InvalidArgument("no code roots are configured (config key \"code\")".into()));
        }
        let name = arg_str(a, "name")?.trim();
        if name.is_empty() || name.len() > 256 {
            return Err(Error::InvalidArgument("name must be 1 to 256 bytes".into()));
        }
        let refs = opt_uint(a, "limit", DEFAULT_REFS, MAX_ROWS)?;
        let lim = Limits { defs: DEFAULT_DEFS.max(refs / 4), refs, impls: DEFAULT_IMPLS.max(refs / 4) };
        let t0 = std::time::Instant::now();
        let ans = code::symbol(self.store.conn(), name, lim)?;
        let query_us = t0.elapsed().as_micros();
        let fresh = self.code_freshness()?;
        let found = ans.totals.iter().any(|&n| n > 0);
        let near = if found { Vec::new() } else { code::near_names(self.store.conn(), &ans.name, 8)? };
        if f.json {
            let sect = |s: i64| Value::Array(ans.hits.iter().filter(|h| h.sect == s).map(hit_json).collect());
            let mut v = json!({
                "name": ans.name,
                "totals": {"defs": ans.totals[0], "refs": ans.totals[1], "impls": ans.totals[2]},
                "defs": sect(code::SECT_DEF),
                "refs": sect(code::SECT_REF),
                "impls": sect(code::SECT_IMPL)
            });
            if !fresh.is_empty() {
                v["freshness"] = json!(fresh);
            }
            if !found {
                v["near"] = Value::Array(near.iter().map(|(n, d)| json!({"name": n, "defs": d})).collect());
            }
            if f.verbose {
                v["elapsed_us"] = json!(query_us as u64);
            }
            return Ok(Answer::Json(v));
        }
        let mut out = if found { code::render(&ans, lim) } else { String::from("not found") };
        if !found {
            if !near.is_empty() {
                out.push_str("; names starting so: ");
                out.push_str(&near.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
            }
            out.push('\n');
        }
        for l in &fresh {
            out.push_str(l);
            out.push('\n');
        }
        if f.verbose {
            out.push_str(&format!("elapsed_us query={query_us}\n"));
        }
        Ok(Answer::Text(out))
    }
}
