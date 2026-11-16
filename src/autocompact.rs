//! Where Claude Code will compact a session on its own, the window that
//! puts that point at atlas's hard limit, and the floor a session is left at
//! after a compaction (atlas-dev#181, ATLAS-CLEAR).
//!
//! Claude Code's threshold, read from the installed build (2.1.323): the
//! compact window is the model's window, clamped by
//! `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (absolute tokens, held to
//! [`MIN_WINDOW`]..=[`MAX_WINDOW`]) or else the `autoCompactWindow` setting;
//! the effective window is that less the smaller of the model's output limit
//! and [`OUTPUT_RESERVE`]; it compacts at that less [`SUMMARY_BUFFER`], or,
//! with `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` set to `p` in (0, 100], at the
//! smaller of `floor(effective * p / 100)` and that. Both are read from the
//! process environment, so one value holds for the main session and every
//! subagent of that process; the window is clamped to each model's own, so
//! an agent on a smaller window keeps its own default point.
//! `DISABLE_AUTO_COMPACT` or `DISABLE_COMPACT` turn automatic compaction off.
//!
//! A subagent past its handoff point under an enforced limit is not
//! compacted at that point: its PreCompact is blocked (review S4 of #192,
//! ATLAS-CLEAR 0008), so it gets the hard limit's deny and hands off.
//!
//! atlas writes the window env from its hard limit (ATLAS-CLEAR 0006), so it
//! is never read as the model's window ([`crate::orchestrate::inferred_window`]).
//!
//! Hooks can speak only on a tool call or a prompt, so the checkpoint prompt
//! is given a margin before the predicted point ([`DEFAULT_MARGIN`]).

use std::path::Path;

use crate::json::Value;
use crate::memory::settings::Settings;

/// What Claude Code keeps back for the model's output when it sizes the
/// effective window (`min(maxOutputTokens, 20000)`; every current model's
/// output limit is larger).
pub const OUTPUT_RESERVE: u64 = 20_000;

/// The headroom Claude Code leaves below the effective window for the
/// summary request itself.
pub const SUMMARY_BUFFER: u64 = 13_000;

/// The percentage override's environment variable (decimals allowed). atlas
/// never writes it, but Claude Code honours it, so the prediction does.
pub const PCT_ENV: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";

/// The auto-compact window's environment variable (tokens), which atlas
/// writes from its hard limit (`atlas settings auto-compact`).
pub const WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

/// The default margin before the predicted point at which the main session
/// is asked for its checkpoint: one large step (ATLAS-CLEAR 0003, "start
/// ~40k").
pub const DEFAULT_MARGIN: u64 = 40_000;

/// The default floor past which a clear is recommended over another
/// compaction (ATLAS-CLEAR 0001, "start ~80k"). Session 9f6780c4's last
/// three compactions left it at 62k, 63k and 69k on its first request.
pub const DEFAULT_CLEAR_FLOOR: u64 = 80_000;

/// Claude Code's smallest auto-compact window: a smaller
/// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` is raised to it.
pub const MIN_WINDOW: u64 = 100_000;

/// Claude Code's largest auto-compact window: a larger one is lowered to it.
pub const MAX_WINDOW: u64 = 1_000_000;

/// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` as Claude Code 2.1.323 reads it (UMe
/// with bare thousands off, then kwn): a `k` or `m` suffix (any case) scales
/// the number before it; else an integer, a whole number in exponent form,
/// or an integer with thousands separators (`,`, `_`, a space). `None`
/// (ignored) when it is not a number or under 1; else held to
/// [`MIN_WINDOW`]..=[`MAX_WINDOW`] and rounded.
pub fn parse_window(v: &str) -> Option<u64> {
    let r = v.trim();
    let mult = match r.chars().last().map(|c| c.to_ascii_lowercase()) {
        Some('m') => Some(1e6),
        Some('k') => Some(1e3),
        _ => None,
    };
    let n = match mult {
        Some(m) => js_float_prefix(r)? * m,
        None => js_int(r)?,
    };
    if n.is_nan() || n < 1.0 {
        return None;
    }
    Some(if n < MIN_WINDOW as f64 {
        MIN_WINDOW
    } else if n > MAX_WINDOW as f64 {
        MAX_WINDOW
    } else {
        n.round() as u64
    })
}

/// JavaScript's `parseFloat`: the longest number at the start of `s`.
fn js_float_prefix(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let digits = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let start = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let int_end = digits(start);
    let mut end = int_end;
    let mut any = int_end > start;
    if b.get(end) == Some(&b'.') {
        let frac_end = digits(end + 1);
        any |= frac_end > end + 1;
        end = frac_end;
    }
    if !any {
        return None;
    }
    if matches!(b.get(end), Some(b'e' | b'E')) {
        let at = end + 1 + usize::from(matches!(b.get(end + 1), Some(b'+' | b'-')));
        let exp_end = digits(at);
        if exp_end > at {
            end = exp_end;
        }
    }
    s[..end].parse::<f64>().ok()
}

/// Claude Code's integer read (`Mp`): a whole number in exponent form, an
/// integer with one kind of thousands separator, else JavaScript's
/// `parseInt(s, 10)` (the leading integer).
fn js_int(s: &str) -> Option<f64> {
    let neg = s.starts_with('-');
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let all_digits = |g: &str| !g.is_empty() && g.bytes().all(|c| c.is_ascii_digit());
    if s.len() <= 32 {
        if let Some((m, e)) = body.split_once(['e', 'E']) {
            let (whole, frac) = m.split_once('.').unwrap_or((m, ""));
            let mantissa = (all_digits(whole) || all_digits(frac)) && (whole.is_empty() || all_digits(whole)) && (frac.is_empty() || all_digits(frac));
            if mantissa && all_digits(e.strip_prefix(['+', '-']).unwrap_or(e)) {
                let n: f64 = s.parse().ok()?;
                return (n.fract() == 0.0).then_some(n);
            }
        }
        if let Some(sep) = body.chars().find(|c| matches!(c, '_' | ',' | ' ' | '\u{a0}' | '\u{202f}')) {
            let mut groups = body.split(sep);
            let first = groups.next().unwrap_or("");
            let rest: Vec<&str> = groups.collect();
            if first.len() <= 3 && all_digits(first) && !rest.is_empty() && rest.iter().all(|g| g.len() == 3 && all_digits(g)) {
                let n: f64 = body.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok()?;
                return Some(if neg { -n } else { n });
            }
        }
    }
    let lead: String = body.chars().take_while(char::is_ascii_digit).collect();
    let n: f64 = lead.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// The `CLAUDE_CODE_AUTO_COMPACT_WINDOW` that has Claude Code compact at
/// `limit` on a model window of `model_window`: the limit plus
/// [`OUTPUT_RESERVE`] and [`SUMMARY_BUFFER`], raised to [`MIN_WINDOW`]
/// (a limit under 67k cannot be reached). `None` when Claude Code already
/// compacts at or before `limit`, or when the window it needs is past
/// [`MAX_WINDOW`].
pub fn window_for(limit: u64, model_window: u64) -> Option<u64> {
    let w = limit.checked_add(OUTPUT_RESERVE + SUMMARY_BUFFER)?;
    (limit > 0 && w < model_window && w <= MAX_WINDOW).then(|| w.max(MIN_WINDOW))
}

/// Claude Code's automatic-compaction inputs, as one process sees them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeCompact {
    /// The model's window ([`crate::orchestrate::inferred_window`]: 200k
    /// with the 1M context off, else 1M; 0: unknown).
    pub window: u64,
    /// `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, when set and usable.
    pub pct: Option<f64>,
    /// `CLAUDE_CODE_AUTO_COMPACT_WINDOW` as Claude Code reads it
    /// ([`parse_window`]), when set and usable.
    pub auto_window: Option<u64>,
    /// Automatic compaction is off (`DISABLE_AUTO_COMPACT`, `DISABLE_COMPACT`).
    pub off: bool,
}

impl ClaudeCompact {
    /// Reads the inputs from an environment.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> ClaudeCompact {
        ClaudeCompact::from_sources(env, None)
    }

    /// Reads the inputs from an environment and Claude Code's user
    /// `settings.json` (its value, when there is one). As in 2.1.323's Pb, a
    /// usable window env wins; else the top-level `autoCompactWindow`
    /// setting, a whole number of tokens in [`MIN_WINDOW`]..=[`MAX_WINDOW`]
    /// ("auto" or anything else: Claude Code's own default). The per-model
    /// `modelSettings` windows and `/autocompact`'s saved ones are not read.
    pub fn from_sources(env: &dyn Fn(&str) -> Option<String>, settings: Option<&Value>) -> ClaudeCompact {
        let setting = settings.and_then(|s| s.get("autoCompactWindow")).and_then(Value::as_u64).filter(|w| (MIN_WINDOW..=MAX_WINDOW).contains(w));
        let set = |k: &str| env(k).is_some_and(|v| !matches!(v.trim(), "" | "0" | "false"));
        ClaudeCompact {
            window: crate::orchestrate::inferred_window(env),
            auto_window: env(WINDOW_ENV).and_then(|v| parse_window(&v)).or(setting),
            pct: env(PCT_ENV).and_then(|v| parse_pct(&v)),
            off: set("DISABLE_AUTO_COMPACT") || set("DISABLE_COMPACT"),
        }
    }

    /// The window Claude Code compacts against: the model's (`configured`,
    /// atlas's `compact.window`, when set, else [`Self::window`]) clamped by
    /// the auto-compact window env. 0 when the model's is unknown.
    pub fn compact_window(&self, configured: u64) -> u64 {
        let model = if configured > 0 { configured } else { self.window };
        self.auto_window.map_or(model, |a| a.min(model))
    }

    /// The point Claude Code compacts at on [`Self::compact_window`]. `None`
    /// when automatic compaction is off or the window is unknown.
    pub fn threshold(&self, configured: u64) -> Option<u64> {
        let w = self.compact_window(configured);
        (!self.off && w > OUTPUT_RESERVE + SUMMARY_BUFFER).then(|| threshold(w, self.pct))
    }
}

/// A usable override: a number in (0, 100].
pub fn parse_pct(v: &str) -> Option<f64> {
    v.trim().parse::<f64>().ok().filter(|p| p.is_finite() && *p > 0.0 && *p <= 100.0)
}

/// The model families atlas knows, as `compact.model_windows` alias keys.
const FAMILIES: [&str; 4] = ["haiku", "opus", "sonnet", "fable"];

/// A model id or override key as it is matched: lowercased and trimmed,
/// with a `[1m]` suffix stripped.
fn normalise(id: &str) -> String {
    let m = id.trim().to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// The context window of `model` (#195, rulings ATLAS-VERSION 0014 and
/// 0016). Keys of `overrides` (`compact.model_windows`) and the id are
/// both normalised ([`normalise`]): an exact id key wins, then a family
/// alias key (`haiku`, `opus`, `sonnet`, `fable`) matching any id of that
/// family. Else by family: a `haiku` model is 200k, an `opus`, `sonnet` or
/// `fable` one has `inferred` ([`crate::orchestrate::inferred_window`]).
/// `None` for any other model, or a family whose inferred window is
/// unknown (0).
pub fn model_window(model: &str, overrides: &std::collections::BTreeMap<String, u64>, inferred: u64) -> Option<u64> {
    let m = normalise(model);
    let keyed = || overrides.iter().map(|(k, w)| (normalise(k), *w));
    if let Some((_, w)) = keyed().find(|(k, _)| *k == m) {
        return Some(w);
    }
    let family = FAMILIES.iter().find(|f| m.contains(*f))?;
    if let Some((_, w)) = keyed().find(|(k, _)| k == family) {
        return Some(w);
    }
    if *family == "haiku" {
        Some(200_000)
    } else {
        (inferred > 0).then_some(inferred)
    }
}

/// The effective window of a compact window.
pub fn effective(window: u64) -> u64 {
    window.saturating_sub(OUTPUT_RESERVE)
}

/// Claude Code's automatic-compaction point for a compact window and an
/// override.
pub fn threshold(window: u64, pct: Option<f64>) -> u64 {
    let e = effective(window);
    let base = e.saturating_sub(SUMMARY_BUFFER);
    match pct {
        Some(p) => ((e as f64 * (p / 100.0)).floor() as u64).min(base),
        None => base,
    }
}

/// The context of the first request after a transcript's newest
/// compaction (what the session restarts from: the summary, the system
/// prompt and tools, and whatever is injected again), else the boundary's
/// `postTokens`. `None` with no compaction in the last 32 MB.
pub fn floor(path: &Path) -> Option<u64> {
    let is_boundary = |l: &str| crate::json::parse(l).is_ok_and(|v| v.get("subtype").and_then(Value::as_str) == Some("compact_boundary"));
    let (_, buf) = crate::checkpoint::last_line_where(path, 32 << 20, "\"compact_boundary\"", is_boundary)?;
    let text = String::from_utf8_lossy(&buf);
    let mut post = None;
    for line in text.lines() {
        let Ok(v) = crate::json::parse(line) else { continue };
        if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            post = v.get("compactMetadata").and_then(|m| m.get("postTokens")).and_then(Value::as_u64);
            continue;
        }
        if v.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(c) = crate::checkpoint::usage_ctx(&v).filter(|c| *c > 0) {
                return Some(c);
            }
        }
    }
    post.filter(|p| *p > 0)
}


/// A main-session segment (the stretch since its last compaction) as the
/// hooks have seen it.
#[derive(Debug, Clone, Default)]
struct Segment {
    /// The newest context seen.
    last_ctx: u64,
    /// The checkpoint was asked for this segment.
    asked: bool,
    /// The context of the last compact prompt (0: none yet).
    warned: u64,
    /// The floor after the compaction that began the segment, once read.
    floor: Option<Option<u64>>,
}

/// The main session's compaction prompts: the checkpoint before a predicted
/// automatic compaction, the advisory /compact (or /clear) prompt of
/// `cap.main_session`, and the once-a-segment deferral of an automatic
/// compaction that comes before the model's checkpoint entry.
#[derive(Debug, Default)]
pub struct MainWatch {
    segments: std::collections::HashMap<std::path::PathBuf, Segment>,
    /// (session, segment start) pairs an automatic compaction already
    /// waited in.
    waited: std::collections::HashSet<(String, String)>,
}

impl MainWatch {
    /// The segment of `path` at `ctx`: a new one once the context has fallen
    /// to under half of the last seen (a compaction or a clear).
    fn segment(&mut self, path: &Path, ctx: u64) -> &mut Segment {
        let s = self.segments.entry(path.to_path_buf()).or_default();
        if ctx < s.last_ctx / 2 {
            *s = Segment::default();
        }
        s.last_ctx = s.last_ctx.max(ctx);
        s
    }

    /// What a main-session tool call or prompt is told at `ctx` context
    /// tokens (empty: nothing).
    pub fn lines(&mut self, s: &Settings, path: &Path, session: &str, ctx: u64) -> String {
        let c = &s.compact;
        let at = if c.enabled && c.checkpoint_prompt { c.claude.threshold(c.window) } else { None };
        let seg = self.segment(path, ctx);
        let mut out: Vec<String> = Vec::new();
        let floor_of = |seg: &mut Segment| *seg.floor.get_or_insert_with(|| floor(path));
        let clear = |f: Option<u64>| f.filter(|f| c.clear_floor > 0 && *f >= c.clear_floor);
        if let Some(at) = at {
            if ctx >= at.saturating_sub(c.checkpoint_margin) && !seg.asked {
                seg.asked = true;
                let mut t = format!(
                    "atlas context: auto-compaction is predicted at {at} tokens and this session is at {ctx}. Write your checkpoint journal entry now (atlas `journal`, lane session:{session}, entry checkpoint): the current step, what is in flight, and the next action; the compaction card names it."
                );
                if let Some(f) = clear(floor_of(seg)) {
                    t.push_str(&format!(
                        " This session was at {f} tokens right after the last compaction (clear floor {}): then ask the user to run /clear instead of letting it compact again; the cleared session's card restores the checkpoint.",
                        c.clear_floor
                    ));
                }
                out.push(t);
            }
        }
        let k = &s.cap;
        let point = if k.main_warn_tokens > 0 { k.main_warn_tokens } else { k.warn_tokens };
        if k.main_session && ctx >= point && (seg.warned == 0 || ctx >= seg.warned + k.warn_every) {
            seg.warned = ctx;
            let at = if ctx >= k.block_tokens { format!("at {ctx} tokens, past its hard limit {}", k.block_tokens) } else { format!("at {ctx} tokens (compact point {point})") };
            let urgent = if ctx >= k.block_tokens { ", before any other work" } else { "" };
            out.push(match clear(floor_of(seg)) {
                Some(f) => format!(
                    "atlas context: the main session is {at}; it was at {f} tokens right after the last compaction (clear floor {}), so another compaction leaves little room. Write your checkpoint journal entry first (atlas `journal`, lane session:{session}, entry checkpoint). Ask the user to run /clear now{urgent}; the cleared session's card restores the checkpoint.",
                    c.clear_floor
                ),
                None => format!("atlas context: the main session is {at}. Ask the user to run /compact now{urgent}."),
            });
        }
        out.join("\n")
    }

    /// Whether an automatic compaction at `ctx` with no checkpoint entry by
    /// the model waits for one: once a segment, only inside the prompt zone,
    /// and only with a margin left before Claude Code's own blocking limit
    /// (the model's effective window less 3,000). `Some(reason)` waits, and the next
    /// tool call or prompt asks for the checkpoint again.
    pub fn wait_for_checkpoint(&mut self, s: &Settings, path: &Path, session: &str, start: &str, ctx: u64) -> Option<String> {
        let c = &s.compact;
        if !(c.enabled && c.checkpoint_prompt) || ctx == 0 {
            return None;
        }
        let at = c.claude.threshold(c.window)?;
        // Blocking is against the model's window, not the compact window
        // (tat in 2.1.323 is given mmt, the model's effective window).
        let window = if c.window > 0 { c.window } else { c.claude.window };
        let ceiling = effective(window).saturating_sub(3_000);
        if ctx < at.saturating_sub(c.checkpoint_margin) || ctx + c.checkpoint_margin >= ceiling {
            return None;
        }
        if !self.waited.insert((session.to_string(), start.to_string())) {
            return None;
        }
        self.segment(path, ctx).asked = false;
        Some(format!("atlas checkpoint-prompt: this automatic compaction waits once for the session's checkpoint journal entry, asked for on its next tool call ({ctx} context tokens, predicted point {at})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_window_reads_the_family_then_the_override() {
        let none = std::collections::BTreeMap::new();
        assert_eq!(model_window("claude-haiku-4-5", &none, 1_000_000), Some(200_000));
        assert_eq!(model_window("Claude-Opus-5-5[1m]", &none, 1_000_000), Some(1_000_000));
        assert_eq!(model_window("sonnet", &none, 200_000), Some(200_000));
        assert_eq!(model_window("claude-fable-5-2", &none, 0), None, "an unknown inferred window");
        assert_eq!(model_window("gpt-x", &none, 1_000_000), None);
        let o: std::collections::BTreeMap<String, u64> = [("gpt-x".to_string(), 128_000)].into_iter().collect();
        assert_eq!(model_window("gpt-x", &o, 1_000_000), Some(128_000));
    }

    /// S1 of the #205 review (ruling ATLAS-VERSION 0016): ids and keys are
    /// lowercased with a `[1m]` suffix stripped, a family alias key matches
    /// any id of its family, and an exact id key beats an alias key.
    #[test]
    fn model_window_overrides_match_normalised_ids_and_family_aliases() {
        let map = |pairs: &[(&str, u64)]| pairs.iter().map(|(k, w)| (k.to_string(), *w)).collect::<std::collections::BTreeMap<_, _>>();
        let exact = map(&[("claude-opus-5-5", 200_000)]);
        assert_eq!(model_window("claude-opus-5-5[1m]", &exact, 1_000_000), Some(200_000), "[1m] stripped from the id");
        assert_eq!(model_window("CLAUDE-OPUS-5-5", &exact, 1_000_000), Some(200_000), "case folded");
        assert_eq!(model_window("claude-opus-5-5", &map(&[("Claude-Opus-5-5[1M]", 300_000)]), 1_000_000), Some(300_000), "the key is normalised too");
        let bedrock = "us.anthropic.claude-opus-5-5-v1:0";
        assert_eq!(model_window(bedrock, &exact, 1_000_000), Some(1_000_000), "no exact match: the family default");
        let alias = map(&[("opus", 400_000), ("Haiku", 500_000)]);
        assert_eq!(model_window(bedrock, &alias, 1_000_000), Some(400_000), "an alias key matches any opus id");
        assert_eq!(model_window("claude-opus-5-5[1m]", &alias, 1_000_000), Some(400_000));
        assert_eq!(model_window("claude-haiku-5-5", &alias, 1_000_000), Some(500_000));
        assert_eq!(model_window("claude-sonnet-4-6", &alias, 1_000_000), Some(1_000_000), "another family keeps its default");
        assert_eq!(model_window("gpt-x", &alias, 1_000_000), None);
        let both = map(&[("opus", 400_000), ("claude-opus-5-5", 200_000)]);
        assert_eq!(model_window("claude-opus-5-5[1m]", &both, 1_000_000), Some(200_000), "an exact id key beats an alias key");
        assert_eq!(model_window("claude-opus-4-1", &both, 1_000_000), Some(400_000));
    }

    #[test]
    fn the_threshold_is_claude_codes() {
        // 1M: 980k effective, 967k without an override.
        assert_eq!(threshold(1_000_000, None), 967_000);
        assert_eq!(threshold(200_000, None), 167_000);
        // The override scales the effective window and only ever lowers it.
        assert_eq!(threshold(1_000_000, Some(50.0)), 490_000);
        assert_eq!(threshold(200_000, Some(50.0)), 90_000);
        assert_eq!(threshold(1_000_000, Some(100.0)), 967_000);
    }

    #[test]
    fn the_window_env_is_parsed_and_clamped_as_claude_code_does() {
        // Claude Code 2.1.323 (UMe with bareThousands off, then kwn): a
        // k or m suffix scales a float, else an integer; under 1 or not a
        // number is ignored; then clamped to [100k, 1M] and rounded.
        assert_eq!(parse_window("483000"), Some(483_000));
        assert_eq!(parse_window(" 483000 "), Some(483_000));
        assert_eq!(parse_window("483k"), Some(483_000));
        assert_eq!(parse_window("483.5K"), Some(483_500));
        assert_eq!(parse_window("1M"), Some(1_000_000));
        assert_eq!(parse_window("1e6"), Some(1_000_000));
        assert_eq!(parse_window("483,000"), Some(483_000));
        assert_eq!(parse_window("50000"), Some(MIN_WINDOW));
        assert_eq!(parse_window("2000000"), Some(MAX_WINDOW));
        assert_eq!(parse_window("1.5m"), Some(MAX_WINDOW));
        assert_eq!(parse_window("0"), None);
        assert_eq!(parse_window("abc"), None);
        assert_eq!(parse_window(""), None);
    }

    #[test]
    fn the_window_for_a_limit_puts_the_point_at_the_limit() {
        // The point is the window less 20k output and 13k buffer.
        assert_eq!(window_for(450_000, 1_000_000), Some(483_000));
        assert_eq!(threshold(483_000, None), 450_000);
        assert_eq!(window_for(400_000, 500_000), Some(433_000));
        assert_eq!(window_for(120_000, 200_000), Some(153_000));
        // Claude Code compacts at or before the limit anyway: nothing to write.
        assert_eq!(window_for(967_000, 1_000_000), None);
        assert_eq!(window_for(970_000, 1_000_000), None);
        assert_eq!(window_for(0, 1_000_000), None);
        // Claude Code's floor: a limit under 67k cannot be reached.
        assert_eq!(window_for(50_000, 1_000_000), Some(MIN_WINDOW));
    }

    #[test]
    fn the_environment_is_read_as_claude_code_reads_it() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        let c = ClaudeCompact::from_env(&env(&[(PCT_ENV, "45.918")]));
        assert_eq!(c.threshold(0), Some(449_996));
        assert_eq!(ClaudeCompact::from_env(&env(&[(WINDOW_ENV, "500000")])).threshold(0), Some(467_000));
        // The window env is where Claude Code compacts, clamped to the model's
        // window; it is never taken for the model's window itself.
        let w = ClaudeCompact::from_env(&env(&[(WINDOW_ENV, "483000")]));
        assert_eq!((w.window, w.auto_window, w.threshold(0)), (1_000_000, Some(483_000), Some(450_000)));
        let small = ClaudeCompact::from_env(&env(&[(WINDOW_ENV, "483000"), ("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1")]));
        assert_eq!((small.window, small.threshold(0)), (200_000, Some(167_000)));
        assert_eq!(small.compact_window(0), 200_000);
        assert_eq!(w.compact_window(0), 483_000);
        // atlas's configured window is the model's: the env still clamps it.
        assert_eq!(w.threshold(500_000), Some(450_000));
        assert_eq!(ClaudeCompact::from_env(&env(&[(WINDOW_ENV, "junk")])).auto_window, None);
        assert_eq!(ClaudeCompact::from_env(&env(&[("CLAUDE_CODE_DISABLE_1M_CONTEXT", "1")])).threshold(0), Some(167_000));
        // atlas's configured window wins.
        assert_eq!(ClaudeCompact::from_env(&env(&[])).threshold(500_000), Some(467_000));
        assert_eq!(ClaudeCompact::from_env(&env(&[("DISABLE_AUTO_COMPACT", "1")])).threshold(0), None);
        assert_eq!(ClaudeCompact::from_env(&env(&[(PCT_ENV, "0")])).pct, None);
        assert_eq!(ClaudeCompact::from_env(&env(&[(PCT_ENV, "150")])).pct, None);
    }

    /// Review N4 of #192: with no usable window env, Claude Code 2.1.323
    /// falls back to the `autoCompactWindow` setting (function Pb: the env
    /// first, then the setting unless it is "auto", then its own default).
    #[test]
    fn the_auto_compact_window_setting_is_read_when_the_env_is_not() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        let settings = |t: &str| crate::json::parse(t).unwrap();
        let s = settings(r#"{"autoCompactWindow": 483000}"#);
        let c = ClaudeCompact::from_sources(&env(&[]), Some(&s));
        assert_eq!((c.auto_window, c.threshold(0)), (Some(483_000), Some(450_000)));
        // The env wins; an unusable env falls through to the setting.
        assert_eq!(ClaudeCompact::from_sources(&env(&[(WINDOW_ENV, "533000")]), Some(&s)).auto_window, Some(533_000));
        assert_eq!(ClaudeCompact::from_sources(&env(&[(WINDOW_ENV, "junk")]), Some(&s)).auto_window, Some(483_000));
        // "auto", a value outside 100k..1M or not a number: Claude Code's own default.
        for t in [r#"{"autoCompactWindow": "auto"}"#, r#"{"autoCompactWindow": 50000}"#, r#"{"autoCompactWindow": "483000"}"#, "{}"] {
            assert_eq!(ClaudeCompact::from_sources(&env(&[]), Some(&settings(t))).auto_window, None, "{t}");
        }
    }

    #[test]
    fn the_floor_is_the_first_request_after_the_newest_boundary() {
        let dir = std::env::temp_dir().join(format!("atlas-floor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let usage = |c: u64| format!("{{\"type\":\"assistant\",\"message\":{{\"id\":\"m{c}\",\"usage\":{{\"input_tokens\":{c}}}}}}}\n");
        let boundary = |post: u64| format!("{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":{{\"trigger\":\"manual\",\"preTokens\":300000,\"postTokens\":{post}}}}}\n");
        std::fs::write(&p, usage(300_000)).unwrap();
        assert_eq!(floor(&p), None);
        std::fs::write(&p, usage(300_000) + &boundary(10_000) + &usage(70_000) + &usage(250_000) + &boundary(14_000) + &usage(90_000) + &usage(95_000)).unwrap();
        assert_eq!(floor(&p), Some(90_000));
        // No request since the newest compaction: its postTokens.
        std::fs::write(&p, usage(300_000) + &boundary(14_000)).unwrap();
        assert_eq!(floor(&p), Some(14_000));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
