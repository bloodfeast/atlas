//! `cargo xtask attrib TRANSCRIPT.jsonl...`: attribute a Claude Code
//! session's processed tokens to what put them in context.
//!
//! Every assistant turn re-reads the whole context, so a tool result that
//! enters context at turn t of T is processed (T - t) more times. Each piece
//! of context is weighted by size times the turns it stays resident and
//! grouped by source: the startup context (system prompt, tool list, memory,
//! brief), shell reads, build and test output, Read, Grep and Glob, the
//! model's own tool inputs and text, and atlas tools (with `slice` counted
//! apart).
//!
//! Characters are divided by 4 for tokens, so figures are estimates; the
//! shares are what the README reports. A transcript is a session's `.jsonl`
//! under `~/.claude/projects/<project>/`; a subagent's is under
//! `<session>/subagents/agent-<id>.jsonl`. Several give totals over them.
//!
//! The arithmetic follows the Python script it replaced operation for
//! operation (see `py.rs`), so the two print the same table.

use crate::py::{self, Num};
use atlas::json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Usage, printed when no transcript is given.
pub const USAGE: &str = "usage: cargo xtask attrib TRANSCRIPT.jsonl [TRANSCRIPT.jsonl ...]";

const READ_CMD: [&str; 10] = ["grep ", "rg ", "sed -n", "cat ", "head ", "tail ", "Select-String", "Get-Content", "find ", "ls "];
const STARTUP: &str = "startup context (first turn)";

/// An insertion-ordered counter of Python numbers (`collections.Counter`).
#[derive(Default, Debug)]
struct Counter {
    keys: Vec<String>,
    vals: HashMap<String, Num>,
}

impl Counter {
    fn add(&mut self, k: &str, v: Num) {
        match self.vals.get_mut(k) {
            Some(x) => *x = x.add(v),
            None => {
                self.keys.push(k.to_string());
                // `Counter[k] += v` starts from int 0.
                self.vals.insert(k.to_string(), Num::Int(0).add(v));
            }
        }
    }
    fn set(&mut self, k: &str, v: Num) {
        if self.vals.insert(k.to_string(), v).is_none() {
            self.keys.push(k.to_string());
        }
    }
    /// `Counter.update(other)`: `self[k] = other[k] + self.get(k, 0)`.
    fn update(&mut self, o: &Counter) {
        for k in &o.keys {
            let v = o.vals[k];
            match self.vals.get_mut(k) {
                Some(x) => *x = v.add(*x),
                None => {
                    self.keys.push(k.clone());
                    self.vals.insert(k.clone(), v);
                }
            }
        }
    }
    fn get(&self, k: &str) -> Num {
        self.vals.get(k).copied().unwrap_or(Num::Int(0))
    }
    fn values(&self) -> impl Iterator<Item = Num> + '_ {
        self.keys.iter().map(|k| self.vals[k])
    }
    /// `most_common()`: by value, descending, ties in insertion order.
    fn most_common(&self) -> Vec<(String, Num)> {
        let mut v: Vec<(String, Num)> = self.keys.iter().map(|k| (k.clone(), self.vals[k])).collect();
        v.sort_by(|a, b| b.1.f().partial_cmp(&a.1.f()).unwrap_or(std::cmp::Ordering::Equal));
        v
    }
}

/// Python `re.I` equality of a text character with a lowercase ASCII
/// pattern letter: case folding admits the long s and the Kelvin sign too.
fn eq_i(t: char, p: char) -> bool {
    t.to_ascii_lowercase() == p || (p == 's' && t == '\u{17f}') || (p == 'k' && t == '\u{212a}')
}

/// Python's Unicode `\w`.
fn is_word(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Whether `pat` (lowercase) matches case-insensitively at char `i`; with
/// `boundary`, a `\b` must follow it.
fn at(t: &[char], i: usize, pat: &str, boundary: bool) -> bool {
    let p: Vec<char> = pat.chars().collect();
    if i + p.len() > t.len() || !p.iter().enumerate().all(|(j, pc)| eq_i(t[i + j], *pc)) {
        return false;
    }
    if !boundary {
        return true;
    }
    // \b after a word character: the next is not one, or the text ends.
    t.get(i + p.len()).is_none_or(|c| !is_word(*c))
}

/// `re.compile(r"cargo |nextest|\.ps1|\.sh\b|make\b|npm (run|test)|pytest", re.I).search(c)`.
fn is_build(c: &str) -> bool {
    let t: Vec<char> = c.chars().collect();
    (0..t.len()).any(|i| {
        at(&t, i, "cargo ", false)
            || at(&t, i, "nextest", false)
            || at(&t, i, ".ps1", false)
            || at(&t, i, ".sh", true)
            || at(&t, i, "make", true)
            || at(&t, i, "npm run", false)
            || at(&t, i, "npm test", false)
            || at(&t, i, "pytest", false)
    })
}

/// The source a tool call's result is counted under.
fn category(name: &str, inp: &Value) -> String {
    if name.starts_with("mcp__atlas") || name.starts_with("mcp__plugin_atlas") {
        return if name.ends_with("__slice") { "atlas:slice" } else { "atlas" }.into();
    }
    match name {
        "Read" => return "read".into(),
        "Grep" | "Glob" => return "search".into(),
        "Edit" | "Write" | "NotebookEdit" => return "edit-echo".into(),
        "Bash" | "PowerShell" => {
            let c = match inp.get("command") {
                None => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
            };
            if c.trim_start_matches(py::is_space).starts_with("atlas slice") {
                return "atlas:slice".into();
            }
            if is_build(&c) {
                return "build/test/run".into();
            }
            if READ_CMD.iter().any(|t| c.contains(t)) {
                return "shell-read".into();
            }
            return "shell-other".into();
        }
        _ => {}
    }
    format!("other:{name}")
}

/// `len(x)` of a text field: characters of a string, entries of a list or
/// an object, else nothing.
fn py_len(v: Option<&Value>) -> usize {
    match v {
        Some(Value::String(s)) => s.chars().count(),
        Some(Value::Array(a)) => a.len(),
        Some(Value::Object(m)) => m.len(),
        _ => 0,
    }
}

fn result_chars(c: &Value) -> usize {
    match c.get("content") {
        Some(Value::String(s)) => s.chars().count(),
        Some(Value::Array(a)) => a.iter().filter(|i| i.is_object()).map(|i| py_len(i.get("text"))).sum(),
        _ => 0,
    }
}

/// A hashable key for a JSON value used as a dict key (`id`s).
fn key(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => format!("s:{s}"),
        Some(v) => format!("v:{v}"),
    }
}

/// A Python int or float from a usage count.
fn num(v: Option<&Value>) -> Num {
    match v {
        None => Num::Int(0),
        Some(Value::Number(n)) if n.is_integer() => Num::Int(n.as_i64().map(i128::from).unwrap_or_else(|| n.as_u64().unwrap_or(0) as i128)),
        Some(v) => Num::Float(v.as_f64().unwrap_or(0.0)),
    }
}

/// One transcript: token counts by source, weighted by residence, and raw.
fn one(path: &Path) -> Result<(Counter, Counter), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut turns: Vec<Vec<(String, usize)>> = Vec::new();
    let mut pending: HashMap<String, String> = HashMap::new();
    let mut first_ctx: Option<Num> = None;
    let mut seen: HashSet<String> = HashSet::new();
    let empty = Value::Object(Default::default());
    for line in py::text_lines(&text) {
        let Ok(e) = atlas::json::parse(&line) else { continue };
        let Value::Object(_) = e else {
            return Err(format!("{}: a line is not a JSON object", path.display()));
        };
        let msg = match e.get("message") {
            Some(m) if !py::truthy(Some(m)) => &empty,
            Some(m @ Value::Object(_)) => m,
            None => &empty,
            Some(_) => continue,
        };
        let content: &[Value] = msg.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        let ty = e.get("type").and_then(Value::as_str);
        if ty == Some("assistant") {
            let mid = msg.get("id");
            let u = match msg.get("usage") {
                Some(u @ Value::Object(_)) => u,
                _ => &empty,
            };
            if py::truthy(mid) && seen.insert(key(mid)) {
                turns.push(Vec::new());
                if first_ctx.is_none() {
                    first_ctx = Some(
                        num(u.get("input_tokens"))
                            .add(num(u.get("cache_creation_input_tokens")))
                            .add(num(u.get("cache_read_input_tokens"))),
                    );
                }
            }
            for c in content {
                if !c.is_object() || turns.is_empty() {
                    continue;
                }
                let last = turns.last_mut().expect("turns is not empty");
                match c.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = c.get("name").and_then(Value::as_str).unwrap_or("");
                        let inp = c.get("input").filter(|v| py::truthy(Some(*v)) && v.is_object()).unwrap_or(&empty);
                        pending.insert(key(c.get("id")), category(name, inp));
                        last.push(("out:tool-input".into(), py::dumps_len(c.get("input").unwrap_or(&Value::Null))));
                    }
                    Some("text") => last.push(("out:text".into(), py_len(c.get("text")))),
                    Some("thinking") => last.push(("out:thinking".into(), py_len(c.get("thinking")))),
                    _ => {}
                }
            }
        } else if ty == Some("user") && !turns.is_empty() {
            for c in content {
                if c.is_object() && c.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let cat = pending.get(&key(c.get("tool_use_id"))).cloned().unwrap_or_else(|| "?".into());
                    turns.last_mut().expect("turns is not empty").push((cat, result_chars(c)));
                }
            }
        }
    }
    let n = turns.len();
    let (mut weighted, mut raw) = (Counter::default(), Counter::default());
    for (t, items) in turns.iter().enumerate() {
        for (k, ch) in items {
            raw.add(k, Num::Float(*ch as f64 / 4.0));
            weighted.add(k, Num::Float(*ch as f64 / 4.0 * (n - t) as f64));
        }
    }
    // `(first_ctx or 0) * n`
    let fc = first_ctx.unwrap_or(Num::Int(0));
    let fc_n = match fc {
        Num::Int(i) => Num::Int(i * n as i128),
        Num::Float(x) => Num::Float(x * n as f64),
    };
    weighted.set(STARTUP, fc_n);
    raw.set(STARTUP, fc);
    Ok((weighted, raw))
}

/// Runs the attribution over `paths` and returns the report text.
pub fn report(paths: &[String]) -> Result<String, String> {
    let mut total = 0.0f64;
    let (mut tw, mut tr) = (Counter::default(), Counter::default());
    let mut shares: HashMap<String, Vec<f64>> = HashMap::new();
    for p in paths {
        let (w, raw) = one(Path::new(p))?;
        let s = match py::sum(w.values()) {
            // `sum(...) or 1`
            Num::Int(0) => Num::Int(1),
            Num::Float(x) if x == 0.0 => Num::Int(1),
            s => s,
        };
        total += s.f();
        tw.update(&w);
        tr.update(&raw);
        for k in &w.keys {
            shares.entry(k.clone()).or_default().push(w.get(k).f() / s.f());
        }
    }
    let mut out = format!("{} transcripts; weighted total {:.3} G tokens (est)\n", paths.len(), total / 1e9);
    out.push_str(&format!("{:30} {:>6} {:>7} {:>7}\n", "source", "share", "median", "raw M"));
    for (k, v) in tw.most_common() {
        let mut xs = shares.get(&k).cloned().unwrap_or_default();
        xs.resize(paths.len(), 0.0);
        let med = py::median(xs);
        out.push_str(&format!("{:30} {:5.1}% {:6.1}% {:7.2}\n", k, v.f() / total * 100.0, med * 100.0, tr.get(&k).f() / 1e6));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories() {
        let cmd = |c: &str| atlas::json::parse(&format!("{{\"command\": {}}}", atlas::json::to_string(&c.to_string()))).unwrap();
        assert_eq!(category("Bash", &cmd("  atlas slice h1")), "atlas:slice");
        assert_eq!(category("Bash", &cmd("CARGO test")), "build/test/run");
        assert_eq!(category("Bash", &cmd("run.sh")), "build/test/run");
        assert_eq!(category("Bash", &cmd("x.shell")), "shell-other");
        assert_eq!(category("Bash", &cmd("grep -n x")), "shell-read");
        assert_eq!(category("mcp__atlas__slice", &Value::Null), "atlas:slice");
        assert_eq!(category("Task", &Value::Null), "other:Task");
    }
}
