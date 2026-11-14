//! The task judge: a model and effort pick for an agent launch, from a
//! table of cells (`orchestrate.judge.cells`, a local file that is never
//! shipped) keyed by the launch's kind and whether it names a brief.
//!
//! The pick is advisory. [`judge`] gives the cell's scope and unknowns and
//! the routing table's model and effort; the launch hook adds one context
//! line when the launch asks for something else, and never denies or
//! rewrites a launch. The kind column's per-kind overrides stay with the
//! orchestrator.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::json::{self, Value};

/// Scope classes, smallest first.
pub const SCOPE: [&str; 5] = ["trivial", "small", "medium", "large", "extreme"];
/// Unknowns classes, fewest first.
pub const UNKNOWNS: [&str; 4] = ["none", "few", "several", "unknown"];
/// Model tiers, cheapest first.
pub const MODELS: [&str; 4] = ["haiku", "sonnet", "opus", "fable"];
/// Effort tiers, lowest first.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// The kinds the cells are keyed by.
pub const KINDS: [&str; 4] = ["reviewer", "builder", "research", "other"];

/// The judge's settings (`orchestrate.judge`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JudgeSettings {
    /// Add the advisory line at Agent launches (default false).
    pub enabled: bool,
    /// The cells file.
    pub cells: Option<PathBuf>,
    /// The work classifier (`orchestrate.judge.classifier`, default off).
    pub classifier: ClassifierSettings,
}

/// One cell: a key (absent parts match anything) and its classes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    kind: Option<String>,
    has_brief: Option<bool>,
    scope: usize,
    unknowns: usize,
}

/// The cells, looked up most specific first: (kind, has brief), then
/// (kind), then the global cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cells {
    cells: Vec<Cell>,
}

fn class(v: &Value, k: &str, names: &[&str]) -> Result<usize> {
    let s = v.get(k).and_then(Value::as_str).ok_or_else(|| Error::InvalidArgument(format!("cell has no {k}")))?;
    names.iter().position(|n| *n == s).ok_or_else(|| Error::InvalidArgument(format!("cell {k} {s:?} is not one of {}", names.join(", "))))
}

impl Cells {
    /// Parses a cells file: `{"cells": [{"key": {"kind", "has_brief"},
    /// "scope", "unk"}, ...]}`, a null key part matching anything.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] when the text is not that shape or has no
    /// global cell.
    pub fn parse(text: &str) -> Result<Cells> {
        let v = json::parse(text).map_err(|e| Error::InvalidArgument(format!("cells: {e:?}")))?;
        let list = v.get("cells").and_then(Value::as_array).ok_or_else(|| Error::InvalidArgument("cells: no cells array".into()))?;
        let mut cells = Vec::with_capacity(list.len());
        for c in list {
            let key = c.get("key");
            cells.push(Cell {
                kind: key.and_then(|k| k.get("kind")).and_then(Value::as_str).map(str::to_string),
                has_brief: key.and_then(|k| k.get("has_brief")).and_then(Value::as_bool),
                scope: class(c, "scope", &SCOPE)?,
                unknowns: class(c, "unk", &UNKNOWNS)?,
            });
        }
        if !cells.iter().any(|c| c.kind.is_none() && c.has_brief.is_none()) {
            return Err(Error::InvalidArgument("cells: no global cell".into()));
        }
        Ok(Cells { cells })
    }

    /// Reads and parses a cells file.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when it cannot be read, else as [`Cells::parse`].
    pub fn load(path: &Path) -> Result<Cells> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::NotFound(format!("judge cells {}: {e}", path.display())))?;
        Cells::parse(&text)
    }

    /// The (scope, unknowns) classes for a launch.
    pub fn lookup(&self, kind: &str, has_brief: bool) -> (usize, usize) {
        let want = [(Some(kind), Some(has_brief)), (Some(kind), None), (None, None)];
        for (k, b) in want {
            if let Some(c) = self.cells.iter().find(|c| c.kind.as_deref() == k && c.has_brief == b) {
                return (c.scope, c.unknowns);
            }
        }
        (0, 0)
    }
}

/// A pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// The scope class.
    pub scope: &'static str,
    /// The unknowns class.
    pub unknowns: &'static str,
    /// The model the routing table gives.
    pub model: &'static str,
    /// The effort the routing table gives.
    pub effort: &'static str,
    /// Scope large or extreme with unknown unknowns: split the task.
    pub split: bool,
    /// What made the pick.
    pub source: &'static str,
}

/// The routing table: scope sets the model, unknowns the effort; large
/// work runs at high effort at least, extreme work at medium.
pub fn route(scope: usize, unknowns: usize) -> (&'static str, &'static str) {
    let model = ["haiku", "sonnet", "opus", "opus", "fable"][scope.min(4)];
    let effort = match scope {
        4 => "medium",
        3 => EFFORTS[unknowns.clamp(2, 3)],
        _ => EFFORTS[unknowns.min(3)],
    };
    (model, effort)
}

/// Removes every line whose first non-space text is `Judge:`.
pub fn strip_judge(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (k, line) in text.split('\n').enumerate() {
        if k > 0 {
            out.push('\n');
        }
        if !line.trim_start().starts_with("Judge:") {
            out.push_str(line);
        }
    }
    out
}

/// Content tokens per window: a 512-token sequence less `[CLS]` and `[SEP]`.
pub const WINDOW: usize = 510;
/// Windows encoded at most; tokens past them are not read.
pub const MAX_WINDOWS: usize = 8;
/// Names the input-vector recipe, for files that store vectors made by it.
pub const RECIPE: &str = "strip Judge lines; wordpiece, no cut; windows of 510, no overlap, at most 8; passage encode each; mean; L2";

/// The input vector of one launch text (see the module docs): `model.dim`
/// floats of unit length.
///
/// # Arguments
///
/// * `model` - the embedder.
/// * `text` - the launch text (prompt, then the brief's text when there is
///   one).
/// * `s` - scratch buffers, reused across calls on one thread.
pub fn input_vector(model: &crate::embed::Model, text: &str, s: &mut crate::embed::bert::Scratch) -> Vec<f32> {
    let pieces = model.tok.encode_all(&strip_judge(text));
    // Room for [CLS] and [SEP] inside the model's sequence length.
    let window = WINDOW.min(model.max_seq.saturating_sub(2)).max(1);
    let mut chunks: Vec<&[u32]> = pieces.chunks(window).take(MAX_WINDOWS).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let mut sum = vec![0f32; model.dim];
    let mut one = vec![0f32; model.dim];
    let mut ids = Vec::with_capacity(window + 2);
    for c in &chunks {
        ids.clear();
        ids.push(model.tok.cls);
        ids.extend_from_slice(c);
        ids.push(model.tok.sep);
        model.bert.encode(model.isa, &ids, s, &mut one);
        for (a, b) in sum.iter_mut().zip(&one) {
            *a += b;
        }
    }
    let n = chunks.len() as f32;
    for a in &mut sum {
        *a /= n;
    }
    let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
    for a in &mut sum {
        *a /= norm;
    }
    sum
}

/// Input vectors for a JSONL text (`{"id": ..., "text": ...}` per line, blank
/// lines skipped), on `threads` threads, in input order. `progress` is
/// called with the count done after each row.
///
/// # Arguments
///
/// * `model` - the embedder.
/// * `jsonl` - the lines.
/// * `threads` - worker threads (at least one).
/// * `progress` - called with (done, total).
///
/// # Errors
///
/// [`Error::InvalidArgument`] when a line is not an object with string `id`
/// and `text`.
pub fn embed_jsonl(
    model: &crate::embed::Model,
    jsonl: &str,
    threads: usize,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<Vec<(String, Vec<f32>)>> {
    let mut rows: Vec<(String, String)> = Vec::new();
    for (k, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v = json::parse(line).map_err(|e| Error::InvalidArgument(format!("line {}: {e:?}", k + 1)))?;
        match (v.get("id").and_then(Value::as_str), v.get("text").and_then(Value::as_str)) {
            (Some(i), Some(t)) => rows.push((i.to_string(), t.to_string())),
            _ => return Err(Error::InvalidArgument(format!("line {}: needs string id and text", k + 1))),
        }
    }
    let total = rows.len();
    let threads = threads.clamp(1, total.max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); total];
    let slots: Vec<std::sync::Mutex<&mut Vec<f32>>> = out.iter_mut().map(std::sync::Mutex::new).collect();
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let mut s = crate::embed::bert::Scratch::default();
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= total {
                        break;
                    }
                    let v = input_vector(model, &rows[i].1, &mut s);
                    if let Ok(mut slot) = slots[i].lock() {
                        **slot = v;
                    }
                    progress(done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1, total);
                }
            });
        }
    });
    drop(slots);
    Ok(rows.into_iter().map(|(i, _)| i).zip(out).collect())
}

/// Whether a prompt names a brief, as [`brief_path`] reads one.
pub fn names_brief(prompt: &str) -> bool {
    brief_path(prompt).is_some()
}

/// The cells' kind for a launch: its `Kind:` line (a brief writer is
/// research), else research for an Explore or Plan agent, else reviewer
/// when the description says review, else other.
pub fn launch_kind(prompt: &str, subagent_type: &str, description: &str) -> &'static str {
    match crate::orchestrate::brief_kind(prompt) {
        Some(crate::orchestrate::Kind::Builder) => "builder",
        Some(crate::orchestrate::Kind::Reviewer) => "reviewer",
        Some(crate::orchestrate::Kind::Research) => "research",
        // The judge has no brief-writer cell; brief writing is design work.
        Some(crate::orchestrate::Kind::BriefWriter) => "research",
        Some(crate::orchestrate::Kind::None) => "other",
        None if subagent_type.eq_ignore_ascii_case("Explore") || subagent_type.eq_ignore_ascii_case("Plan") => "research",
        None if description.to_ascii_lowercase().contains("review") => "reviewer",
        None => "other",
    }
}

/// The pick for a launch prompt (its `Judge:` lines are ignored).
pub fn judge(cells: &Cells, prompt: &str, kind: &str) -> Pick {
    let (s, u) = cells.lookup(kind, names_brief(&strip_judge(prompt)));
    let (model, effort) = route(s, u);
    Pick { scope: SCOPE[s], unknowns: UNKNOWNS[u], model, effort, split: s >= 3 && u == 3, source: "rule A" }
}

/// The line to paste into a brief.
pub fn line(p: &Pick) -> String {
    let split = if p.split { "; split: large with unknown unknowns, break the task up" } else { "" };
    format!(
        "Judge: scope {}, unknowns {} -> {}, {} (atlas {}; kind column: orchestrator{split}).",
        p.scope, p.unknowns, p.model, p.effort, p.source
    )
}

/// The model and effort a prompt's own `Judge:` line states, if any.
pub fn stated(prompt: &str) -> Option<(String, String)> {
    let l = prompt.lines().map(str::trim_start).find(|l| l.starts_with("Judge:"))?;
    let (_, pick) = l.split_once("->")?;
    let mut it = pick.split(',').map(|w| w.trim().trim_end_matches('.').split_whitespace().next().unwrap_or("").to_ascii_lowercase());
    let model = it.next().filter(|m| MODELS.contains(&m.as_str()))?;
    let effort = it.next().filter(|e| EFFORTS.contains(&e.as_str()))?;
    Some((model, effort))
}

fn tier_of(model: &str) -> Option<usize> {
    let m = model.to_ascii_lowercase();
    MODELS.iter().position(|t| m.contains(t))
}

/// The advisory line for a launch, when its asked model or its `Judge:`
/// line differs from the pick by a tier; `None` when they agree or there
/// is nothing to compare.
pub fn note(p: &Pick, asked_model: &str, prompt: &str) -> Option<String> {
    let pm = tier_of(p.model);
    let mut why = Vec::new();
    if let Some(t) = tier_of(asked_model).filter(|t| Some(*t) != pm) {
        why.push(format!("the launch asks {}", MODELS[t]));
    }
    if let Some((m, e)) = stated(prompt) {
        if m != p.model || e != p.effort {
            why.push(format!("its Judge line says {m}, {e}"));
        }
    }
    (!why.is_empty()).then(|| format!("atlas judge (advisory): {}; the pick is {}, {}. {}", why.join(" and "), p.model, p.effort, line(p)))
}

// The work classifier (rule C): an ordinal demand score 0-5 per brief from a
// small head over the input vector, and an editable rules map from score and
// kind to model and effort. Advisory only, default off.

/// The encoder the head was trained on (`Model::id` of bge-int8.atm).
pub const CLASSIFIER_MODEL: &str = "746bdb58bc259433-int8";
/// Input vector width the head reads.
pub const CLASSIFIER_DIM: usize = 384;
/// Score classes, 0 to 5.
pub const SCORES: usize = 6;
/// The kind-only table: the modal training score per kind ([`KINDS`]
/// order), the fallback score source when the head failed its bar.
pub const KIND_TABLE: [usize; 4] = [0, 5, 2, 0];

/// Where the classifier's score comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScoreSource {
    /// The trained head over the brief's input vector.
    Head,
    /// The kind-only table ([`KIND_TABLE`]).
    #[default]
    Kind,
}

/// One per-kind override: at or under `max_score`, `pick` replaces the base
/// map's entry, when `enabled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    /// Applied only when true.
    pub enabled: bool,
    /// The highest score it applies to.
    pub max_score: usize,
    /// The model and effort it gives.
    pub pick: (String, String),
}

/// The rules map: score and kind to model and effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesMap {
    /// The pick per score, 0 to 5.
    pub base: Vec<(String, String)>,
    /// The research override (present, default off).
    pub research: Option<Override>,
    /// The builder override (none until the builder replays set it).
    pub builder: Option<Override>,
}

impl Default for RulesMap {
    fn default() -> Self {
        let pair = |m: &str, e: &str| (m.to_string(), e.to_string());
        let mut base = vec![pair("opus", "medium"); SCORES];
        base[5] = pair("opus", "high");
        RulesMap { base, research: Some(Override { enabled: false, max_score: 1, pick: pair("sonnet", "medium") }), builder: None }
    }
}

/// The classifier's settings (`orchestrate.judge.classifier`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassifierSettings {
    /// Run the classifier (default false).
    pub enabled: bool,
    /// The head's weights file (`weights.f32`).
    pub weights: Option<PathBuf>,
    /// The weights file's sha256, checked when set.
    pub weights_sha256: Option<String>,
    /// The score source.
    pub source: ScoreSource,
    /// The rules map.
    pub map: RulesMap,
}

/// Splits `model/effort`; `None` unless both are known tiers.
pub fn parse_pick(s: &str) -> Option<(String, String)> {
    let (m, e) = s.split_once('/')?;
    let (m, e) = (m.trim(), e.trim());
    (MODELS.contains(&m) && EFFORTS.contains(&e)).then(|| (m.to_string(), e.to_string()))
}

/// The rules map's pick for a score and kind: the kind's enabled override
/// when the score is at or under its cut, else the base entry.
pub fn rules(score: usize, kind: &str, map: &RulesMap) -> (String, String) {
    let score = score.min(SCORES - 1);
    let over = match kind {
        "research" => map.research.as_ref(),
        "builder" => map.builder.as_ref(),
        _ => None,
    };
    if let Some(o) = over.filter(|o| o.enabled && score <= o.max_score) {
        return o.pick.clone();
    }
    map.base.get(score).or(map.base.last()).cloned().unwrap_or_else(|| ("opus".into(), "medium".into()))
}

/// The ordinal head: `w` over the input vector then the kind one-hot, and
/// ascending thresholds; the score is the count of thresholds below `x . w`.
#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    w: Vec<f32>,
    th: Vec<f32>,
}

impl Head {
    /// The head from the bytes of a weights file (f32 little-endian, `w`
    /// then [`SCORES`] - 1 thresholds); `None` when the size or the sha256
    /// (when given) does not match.
    pub fn parse(bytes: &[u8], sha256: Option<&str>) -> Option<Head> {
        let nw = CLASSIFIER_DIM + KINDS.len();
        if bytes.len() != (nw + SCORES - 1) * 4 {
            return None;
        }
        if let Some(want) = sha256 {
            if !crate::sha256::hex(bytes).eq_ignore_ascii_case(want.trim()) {
                return None;
            }
        }
        let f: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        Some(Head { w: f[..nw].to_vec(), th: f[nw..].to_vec() })
    }

    /// The head from a file; `None` when it is missing or does not parse.
    pub fn load(path: &Path, sha256: Option<&str>) -> Option<Head> {
        Head::parse(&std::fs::read(path).ok()?, sha256)
    }

    /// The raw score `x . w`, accumulated in f64.
    pub fn dot(&self, v: &[f32], kind: &str) -> f64 {
        let mut s: f64 = self.w.iter().zip(v).map(|(a, b)| f64::from(*a) * f64::from(*b)).sum();
        if let Some(k) = KINDS.iter().position(|x| *x == kind) {
            s += f64::from(self.w[CLASSIFIER_DIM + k]);
        }
        s
    }

    /// The score class, 0 to 5.
    pub fn score(&self, v: &[f32], kind: &str) -> usize {
        let s = self.dot(v, kind);
        self.th.iter().filter(|t| s > f64::from(**t)).count()
    }
}

/// The kind-only score.
pub fn kind_score(kind: &str) -> usize {
    KINDS.iter().position(|x| *x == kind).map_or(KIND_TABLE[3], |k| KIND_TABLE[k])
}

/// The first brief path a prompt names: a rooted path (drive-rooted, `C:/`
/// or `C:\`, or Unix-rooted, `/`) to a `.md` file whose name starts with
/// `BRIEF` (a brief review's file does not count).
pub fn brief_path(prompt: &str) -> Option<PathBuf> {
    prompt
        .split(|c: char| c.is_whitespace() || "\"'`<>|*?,;()[]".contains(c))
        .map(|w| w.trim_end_matches(['.', ':', '!']))
        .find(|w| {
            let name = w.rsplit(['/', '\\']).next().unwrap_or("").to_ascii_uppercase();
            rooted(w) && w.to_ascii_lowercase().ends_with(".md") && name.starts_with("BRIEF") && !name.starts_with("BRIEF_REVIEW")
        })
        .map(PathBuf::from)
}

/// Whether a word is a rooted path: a drive and separator (`C:/`, `C:\`)
/// or a Unix root (`/`) with something after it.
fn rooted(w: &str) -> bool {
    let b = w.as_bytes();
    let drive = b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\');
    drive || (b.len() > 1 && b[0] == b'/')
}

/// A classifier pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassPick {
    /// The demand score, 0 to 5.
    pub score: usize,
    /// "head" or "kind".
    pub source: &'static str,
    /// The rules map's model.
    pub model: String,
    /// The rules map's effort.
    pub effort: String,
}

/// The classifier's pick for a launch, or `None` when it is off, the launch
/// names no readable brief, or (source head) the weights or the encoder do
/// not load or match. Never an error. `model` is the loaded encoder, if any.
pub fn classify(
    s: &ClassifierSettings,
    prompt: &str,
    kind: &str,
    model: Option<&crate::embed::Model>,
    scratch: &mut crate::embed::bert::Scratch,
) -> Option<ClassPick> {
    if !s.enabled {
        return None;
    }
    let path = brief_path(&strip_judge(prompt))?;
    let (score, source) = match s.source {
        ScoreSource::Kind => (kind_score(kind), "kind"),
        ScoreSource::Head => {
            let head = Head::load(s.weights.as_deref()?, s.weights_sha256.as_deref())?;
            let model = model.filter(|m| m.id == CLASSIFIER_MODEL && m.dim == CLASSIFIER_DIM)?;
            let text = std::fs::read_to_string(&path).ok()?;
            (head.score(&input_vector(model, &text, scratch), kind), "head")
        }
    };
    let (m, e) = rules(score, kind, &s.map);
    Some(ClassPick { score, source, model: m, effort: e })
}

/// The advisory line for a classifier pick.
pub fn class_line(p: &ClassPick) -> String {
    format!("Classifier: demand score {} ({}) -> {}, {} (atlas rule C; advisory).", p.score, p.source, p.model, p.effort)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(m: &str, e: &str) -> (String, String) {
        (m.to_string(), e.to_string())
    }

    #[test]
    fn every_rules_map_cell() {
        let off = RulesMap::default();
        let mut research_on = RulesMap::default();
        research_on.research.as_mut().unwrap().enabled = true;
        let builder_set =
            RulesMap { builder: Some(Override { enabled: true, max_score: 2, pick: pair("sonnet", "high") }), ..RulesMap::default() };
        for score in 0..SCORES {
            let base = if score == 5 { pair("opus", "high") } else { pair("opus", "medium") };
            for kind in KINDS {
                // Default map: no override applies (research is present but off,
                // builder is null, reviewer has none).
                assert_eq!(rules(score, kind, &off), base, "{score} {kind} off");
                let want = if kind == "research" && score <= 1 { pair("sonnet", "medium") } else { base.clone() };
                assert_eq!(rules(score, kind, &research_on), want, "{score} {kind} research on");
                let want = if kind == "builder" && score <= 2 { pair("sonnet", "high") } else { base.clone() };
                assert_eq!(rules(score, kind, &builder_set), want, "{score} {kind} builder set");
            }
        }
        assert!(off.builder.is_none() && off.research.as_ref().is_some_and(|o| !o.enabled));
        let mut b_off = builder_set.clone();
        b_off.builder.as_mut().unwrap().enabled = false;
        assert_eq!(rules(0, "builder", &b_off), pair("opus", "medium"));
        assert_eq!(rules(9, "reviewer", &off), pair("opus", "high"));
        assert_eq!(parse_pick("sonnet/medium"), Some(pair("sonnet", "medium")));
        assert_eq!(parse_pick("gpt/medium"), None);
    }

    fn weights_bytes() -> Vec<u8> {
        let nw = CLASSIFIER_DIM + KINDS.len();
        let mut f = vec![0f32; nw];
        f[0] = 1.0;
        f[CLASSIFIER_DIM + 1] = 0.5; // builder
        f.extend_from_slice(&[-0.5, 0.0, 0.25, 0.75, 1.25]);
        f.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn missing_truncated_and_wrong_sha_weights_give_no_head() {
        let b = weights_bytes();
        let sha = crate::sha256::hex(&b);
        let h = Head::parse(&b, Some(&sha)).unwrap();
        let mut v = vec![0f32; CLASSIFIER_DIM];
        v[0] = 0.5;
        assert_eq!(h.score(&v, "reviewer"), 3); // 0.5 > -0.5, 0, 0.25
        assert_eq!(h.score(&v, "builder"), 4); // 1.0 > 0.75
        assert!(Head::parse(&b, None).is_some());
        assert!(Head::parse(&b[..b.len() - 4], None).is_none());
        assert!(Head::parse(&b, Some(&"0".repeat(64))).is_none());
        let dir = std::env::temp_dir().join(format!("atlas-cls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Head::load(&dir.join("absent.f32"), None).is_none());
        let p = dir.join("w.f32");
        std::fs::write(&p, &b[..100]).unwrap();
        assert!(Head::load(&p, None).is_none());
        // Through classify: on, head source, bad weights: no pick, no panic.
        let brief = dir.join("BRIEF_X.md");
        std::fs::write(&brief, "x").unwrap();
        let prompt = format!("Read {}", brief.display()).replace('\\', "/");
        let s = ClassifierSettings { enabled: true, source: ScoreSource::Head, weights: Some(p.clone()), ..Default::default() };
        let mut sc = crate::embed::bert::Scratch::default();
        assert!(classify(&s, &prompt, "builder", None, &mut sc).is_none());
        let s = ClassifierSettings { weights: Some(dir.join("absent.f32")), ..s };
        assert!(classify(&s, &prompt, "builder", None, &mut sc).is_none());
        // Off: nothing. Kind source: the table. No brief named: nothing.
        let k = ClassifierSettings { enabled: true, ..Default::default() };
        assert!(classify(&ClassifierSettings::default(), &prompt, "builder", None, &mut sc).is_none());
        assert_eq!(classify(&k, &prompt, "reviewer", None, &mut sc).map(|p| (p.score, p.source)), Some((0, "kind")));
        assert_eq!(classify(&k, &prompt, "research", None, &mut sc).map(|p| p.score), Some(2));
        assert!(classify(&k, "no brief here", "builder", None, &mut sc).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The trained head against the trainer's golden rows. Needs the local
    /// artifacts under H:/models/taskjudge/classifier/head_c; skips when they
    /// are absent.
    #[test]
    fn golden_head_scores_equal_the_trainer() {
        let dir = std::path::Path::new("H:/models/taskjudge/classifier/head_c");
        let (Ok(meta), Ok(vj), Ok(vb), Ok(wb)) = (
            std::fs::read_to_string(dir.join("head_c.json")),
            std::fs::read_to_string(dir.join("vectors.json")),
            std::fs::read(dir.join("vectors.f32")),
            std::fs::read(dir.join("weights.f32")),
        ) else {
            eprintln!("skipped: the head_c artifacts are not on this machine");
            return;
        };
        let meta = json::parse(&meta).unwrap();
        let vj = json::parse(&vj).unwrap();
        let head = Head::parse(&wb, meta.get("weights_sha256").and_then(Value::as_str)).expect("weights match head_c.json");
        let ids: Vec<&str> = vj.get("ids").and_then(Value::as_array).unwrap().iter().filter_map(Value::as_str).collect();
        let golden = meta.get("golden").and_then(Value::as_array).unwrap();
        assert_eq!(golden.len(), 5);
        for g in golden {
            let id = g.get("id").and_then(Value::as_str).unwrap();
            let kind = g.get("kind").and_then(Value::as_str).unwrap();
            let at = ids.iter().position(|x| *x == id).unwrap();
            let v: Vec<f32> = vb[at * CLASSIFIER_DIM * 4..(at + 1) * CLASSIFIER_DIM * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect();
            let want_dot = g.get("dot").and_then(Value::as_f64).unwrap();
            assert!((head.dot(&v, kind) - want_dot).abs() < 1e-6, "{id}");
            assert_eq!(head.score(&v, kind) as u64, g.get("class").and_then(Value::as_u64).unwrap(), "{id}");
        }
    }

    /// A cells file of the shape the judge reads; values are made up.
    const CELLS: &str = r#"{"cells": [
        {"key": {"kind": null, "has_brief": null}, "scope": "trivial", "unk": "unknown"},
        {"key": {"kind": "builder", "has_brief": null}, "scope": "medium", "unk": "unknown"},
        {"key": {"kind": "builder", "has_brief": true}, "scope": "large", "unk": "unknown"},
        {"key": {"kind": "builder", "has_brief": false}, "scope": "medium", "unk": "few"},
        {"key": {"kind": "reviewer", "has_brief": true}, "scope": "trivial", "unk": "none"},
        {"key": {"kind": "reviewer", "has_brief": null}, "scope": "small", "unk": "none"},
        {"key": {"kind": "research", "has_brief": null}, "scope": "large", "unk": "several"},
        {"key": {"kind": "other", "has_brief": false}, "scope": "extreme", "unk": "few"}
    ]}"#;

    /// (kind, has brief) -> scope, unknowns, model, effort, split: the same
    /// lookup and routing table in the reference script, 20 cases.
    const GOLDEN: [(&str, bool, &str, &str, &str, &str, bool); 20] = [
        ("builder", true, "large", "unknown", "opus", "xhigh", true),
        ("builder", false, "medium", "few", "opus", "medium", false),
        ("reviewer", true, "trivial", "none", "haiku", "low", false),
        ("reviewer", false, "small", "none", "sonnet", "low", false),
        ("research", true, "large", "several", "opus", "high", false),
        ("research", false, "large", "several", "opus", "high", false),
        ("other", false, "extreme", "few", "fable", "medium", false),
        ("other", true, "trivial", "unknown", "haiku", "xhigh", false),
        ("none", true, "trivial", "unknown", "haiku", "xhigh", false),
        ("none", false, "trivial", "unknown", "haiku", "xhigh", false),
        ("builder", true, "large", "unknown", "opus", "xhigh", true),
        ("reviewer", true, "trivial", "none", "haiku", "low", false),
        ("research", true, "large", "several", "opus", "high", false),
        ("other", false, "extreme", "few", "fable", "medium", false),
        ("builder", false, "medium", "few", "opus", "medium", false),
        ("reviewer", false, "small", "none", "sonnet", "low", false),
        ("other", true, "trivial", "unknown", "haiku", "xhigh", false),
        ("research", false, "large", "several", "opus", "high", false),
        ("builder", true, "large", "unknown", "opus", "xhigh", true),
        ("none", false, "trivial", "unknown", "haiku", "xhigh", false),
    ];

    #[test]
    fn golden_picks_match_the_reference_lookup() {
        let cells = Cells::parse(CELLS).unwrap();
        for (k, (kind, brief, s, u, m, e, split)) in GOLDEN.iter().enumerate() {
            let prompt = if *brief {
                "Kind: x\nDo the work in Z:/work/BRIEF_THING.md now."
            } else {
                "Do the work; see Z:/work/brief_review_THING.md."
            };
            let p = judge(&cells, prompt, kind);
            assert_eq!((p.scope, p.unknowns, p.model, p.effort, p.split, p.source), (*s, *u, *m, *e, *split, "rule A"), "case {k}");
        }
    }

    #[test]
    fn the_routing_table() {
        assert_eq!(route(0, 0), ("haiku", "low"));
        assert_eq!(route(1, 2), ("sonnet", "high"));
        assert_eq!(route(2, 3), ("opus", "xhigh"));
        assert_eq!(route(3, 0), ("opus", "high"));
        assert_eq!(route(4, 3), ("fable", "medium"));
    }

    #[test]
    fn a_missing_or_bad_file_gives_no_cells() {
        let dir = std::env::temp_dir().join(format!("atlas-judge-{}", std::process::id()));
        assert!(matches!(Cells::load(&dir.join("absent.json")), Err(Error::NotFound(_))));
        assert!(Cells::parse("{\"cells\": []}").is_err(), "no global cell");
        assert!(Cells::parse("{\"cells\": [{\"key\": {}, \"scope\": \"huge\", \"unk\": \"none\"}]}").is_err());
    }

    #[test]
    fn judge_lines_are_stripped_and_read() {
        let t = "do the thing\n  Judge: scope small, unknowns none -> sonnet, low\nnot a Judge: line\nJudge:x";
        assert_eq!(strip_judge(t), "do the thing\n\nnot a Judge: line\n");
        assert_eq!(stated(t), Some(("sonnet".into(), "low".into())));
        assert_eq!(stated("Judge: scope large, unknowns several -> opus, high."), Some(("opus".into(), "high".into())));
        assert_eq!(stated("no line"), None);
        // A brief path inside a Judge line does not count as naming a brief.
        let cells = Cells::parse(CELLS).unwrap();
        assert_eq!(judge(&cells, "Judge: see Z:/w/BRIEF_X.md -> opus, high", "builder").scope, "medium");
    }

    #[test]
    fn brief_paths_and_kinds() {
        assert!(names_brief("Read C:\\work\\BRIEF_ONE.md in full."));
        assert!(names_brief("brief: \"Z:/a/b/brief-two.md\""));
        assert!(!names_brief("Read Z:/a/BRIEF_REVIEW_ONE.md"));
        assert!(!names_brief("Read BRIEF_ONE.md"), "not rooted");
        assert!(names_brief("Read /tmp/work/BRIEF_ONE.md in full."), "Unix-rooted");
        assert_eq!(brief_path("See /x/BRIEF_ONE.md."), Some(PathBuf::from("/x/BRIEF_ONE.md")));
        assert!(!names_brief("Read /x/BRIEF_REVIEW_ONE.md"));
        assert!(!names_brief("/compact"), "a slash command");
        assert_eq!(brief_path("Read BRIEF_ONE.md"), None, "not rooted");
        assert_eq!(launch_kind("Kind: reviewer\nRead it.", "", ""), "reviewer");
        assert_eq!(launch_kind("Find the callers.", "Explore", ""), "research");
        assert_eq!(launch_kind("Look at the PR.", "", "PR review"), "reviewer");
        assert_eq!(launch_kind("Do it.", "", "fix"), "other");
    }

    #[test]
    fn the_note_is_advisory_and_only_on_a_difference() {
        let cells = Cells::parse(CELLS).unwrap();
        let prompt = "Kind: builder\nJudge: scope large, unknowns unknown -> opus, xhigh\nSee Z:/w/BRIEF_X.md";
        let p = judge(&cells, prompt, "builder");
        assert_eq!(note(&p, "opus", prompt), None, "agrees");
        assert_eq!(note(&p, "", prompt), None, "no model asked");
        let n = note(&p, "sonnet", prompt).unwrap();
        assert!(n.starts_with("atlas judge (advisory): the launch asks sonnet"), "{n}");
        assert!(n.contains("kind column: orchestrator"));
        let low = "Judge: scope small -> opus, low\nZ:/w/BRIEF_X.md";
        assert!(note(&judge(&cells, low, "builder"), "", low).unwrap().contains("its Judge line says opus, low"));
        assert!(line(&p).contains("split"));
    }
}
