//! The orchestrated-mode question: `atlas --setup`, `atlas settings offer`
//! and `atlas settings orchestrate` ask whether the user wants to work in
//! orchestrated mode, and on yes ask its options and store the answers in
//! atlas's configuration. The first session asks the same through Claude
//! ([`orchestrate_line`]). Nothing is turned on without a yes.
//!
//! ```text
//! atlas settings orchestrate            asks (a terminal), else lists the options
//! atlas settings orchestrate --yes [--builders N] [--reviewers N] [--research N]
//!     [--mode enforced|advisory|off] [--handoff TOKENS] [--limit TOKENS] [--fresh auto|ask]
//! atlas settings orchestrate --no       records the answer; turns the caps off if on
//! ```
//!
//! What is stored: `orchestrate.enabled`, the three caps
//! (`orchestrate.kinds.<kind>.max`), `orchestrate.mode` and
//! `orchestrate.fresh`, each the answer or, for an empty answer, the value
//! in effect. The handoff point and the hard limit (`cap.warn_tokens`,
//! `cap.block_tokens`) are stored only when a number is given: their
//! defaults scale to the model's context window, and a stored number would
//! not.

use super::{check, edit_config_many, get_path, mark_offered, offered, Item, Io, Paths, Written};
use crate::json::Value;
use crate::orchestrate::{DEFAULT_HANDOFF, DEFAULT_LIMIT};
use std::process::ExitCode;

/// The manifest id the question is recorded under (the caps' switch).
pub const ID: &str = "orchestrate";

/// One option: its flag, the manifest entry that checks a value (if any),
/// where it is stored, and its built-in default.
struct Opt {
    flag: &'static str,
    item: Option<&'static str>,
    keys: &'static [&'static str],
    question: &'static str,
    default: Option<&'static str>,
}

const OPTS: [Opt; 7] = [
    Opt { flag: "--builders", item: Some("agents-builders"), keys: &["orchestrate", "kinds", "builder", "max"], question: "Builders at once (agents that write and compile code)", default: Some("2") },
    Opt { flag: "--reviewers", item: Some("agents-reviewers"), keys: &["orchestrate", "kinds", "reviewer", "max"], question: "Reviewers at once (read-only review agents)", default: Some("2") },
    Opt { flag: "--research", item: Some("agents-research"), keys: &["orchestrate", "kinds", "research", "max"], question: "Research and design agents at once", default: Some("1") },
    Opt { flag: "--mode", item: Some("context-mode"), keys: &["orchestrate", "mode"], question: "Context limits: enforced, advisory or off", default: Some("enforced") },
    Opt { flag: "--handoff", item: Some("context-handoff"), keys: &["cap", "warn_tokens"], question: "Handoff point, in context tokens (an agent is told to wrap up from here)", default: None },
    Opt { flag: "--limit", item: Some("context-limit"), keys: &["cap", "block_tokens"], question: "Hard limit, in context tokens (an agent must have handed off by here)", default: None },
    Opt { flag: "--fresh", item: None, keys: &["orchestrate", "fresh"], question: "After a handoff, start the fresh agent: auto, or ask the user first", default: Some("auto") },
];

const HANDOFF: usize = 4;
const LIMIT: usize = 5;

/// What the question says the mode is, before the options.
const ABOUT: &str = "\n[orchestrate] Orchestrated mode
  What: for a session that delegates its work to subagents. atlas caps how many agents of each kind (builders,
  reviewers, research) run at once, denying a launch past its kind's cap with a reason to queue it, and tells each
  agent to wrap up and hand off near its context limit. The orchestrate skill teaches the session to work this way.
  Cost: a launch past a cap waits for a running agent to finish; an agent past its hard limit can only hand off.";

/// The usage line of `atlas settings orchestrate`.
pub const USAGE: &str = "atlas settings orchestrate [--yes [--builders N] [--reviewers N] [--research N] [--mode enforced|advisory|off] [--handoff TOKENS] [--limit TOKENS] [--fresh auto|ask] | --no]";

/// Abbreviations a terminal answer may use.
fn expand(o: &Opt, raw: &str) -> String {
    let r = raw.trim().to_ascii_lowercase();
    let full = match (o.flag, r.as_str()) {
        ("--mode", "e") => "enforced",
        ("--mode", "a") => "advisory",
        ("--mode", "o") => "off",
        ("--fresh", "a") => "auto",
        ("--fresh", "k") => "ask",
        _ => return r.replace([',', '_'], ""),
    };
    full.to_string()
}

/// A value for an option, checked: the manifest's range or choices, or
/// `auto`/`ask` for `--fresh`.
fn parse(items: &[Item], o: &Opt, raw: &str) -> Result<Value, String> {
    let raw = expand(o, raw);
    match o.item {
        Some(id) => {
            let s = items.iter().find(|s| s.id == id).ok_or_else(|| format!("manifest: no {id}"))?;
            check(s, &raw).map_err(|e| format!("{}: {}", o.flag, e.split_once(": ").map_or(e.as_str(), |(_, m)| m)))
        }
        None if matches!(raw.as_str(), "auto" | "ask") => Ok(Value::String(raw)),
        None => Err(format!("{}: choose auto or ask", o.flag)),
    }
}

/// The value in effect for an option: configured, else its default.
fn in_effect(atlas: &Value, o: &Opt) -> Option<Value> {
    get_path(atlas, o.keys).cloned().or_else(|| o.default.map(|d| crate::json::parse(d).unwrap_or_else(|_| Value::String(d.to_string()))))
}

/// How the default of an option reads in a question.
fn default_label(atlas: &Value, o: &Opt, i: usize) -> String {
    match (in_effect(atlas, o), i) {
        (Some(Value::String(s)), _) => s,
        (Some(v), _) => v.to_string(),
        (None, HANDOFF) => format!("{} on a 1M window, scaled to the window", thousands(DEFAULT_HANDOFF)),
        (None, LIMIT) => format!("{} on a 1M window, scaled to the window", thousands(DEFAULT_LIMIT)),
        (None, _) => "not set".into(),
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The answers: one per option, `None` for "keep what is in effect".
type Answers = [Option<Value>; 7];

/// The configuration edits for a yes: the switch, then every option that
/// has an answer or a value in effect (the limits only when answered).
fn edits(atlas: &Value, a: &Answers) -> Vec<(&'static [&'static str], Option<Value>)> {
    let mut out: Vec<(&'static [&'static str], Option<Value>)> = vec![(&["orchestrate", "enabled"], Some(Value::Bool(true)))];
    for (i, o) in OPTS.iter().enumerate() {
        let v = a[i].clone().or_else(|| if o.default.is_some() { in_effect(atlas, o) } else { None });
        if let Some(v) = v {
            out.push((o.keys, Some(v)));
        }
    }
    out
}

/// The handoff point must not be above the hard limit (each as answered,
/// else as configured).
fn check_order(atlas: &Value, a: &Answers) -> Result<(), String> {
    let n = |i: usize| a[i].clone().or_else(|| get_path(atlas, OPTS[i].keys).cloned()).and_then(|v| v.as_f64());
    match (n(HANDOFF), n(LIMIT)) {
        (Some(h), Some(l)) if h > l => Err(format!("the handoff point ({h}) is above the hard limit ({l}); give a handoff point at or below the limit")),
        _ => Ok(()),
    }
}

fn summary(atlas: &Value, a: &Answers) -> String {
    let v = |i: usize| a[i].clone().or_else(|| in_effect(atlas, &OPTS[i])).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string));
    let mut s = format!("builders {}, reviewers {}, research {}, context limits {}", v(0).unwrap_or_default(), v(1).unwrap_or_default(), v(2).unwrap_or_default(), v(3).unwrap_or_default());
    for (i, name) in [(HANDOFF, "handoff"), (LIMIT, "limit")] {
        if let Some(x) = &a[i] {
            s.push_str(&format!(", {name} {x}"));
        }
    }
    s.push_str(&format!(", fresh {}", v(6).unwrap_or_default()));
    s
}

/// Writes the yes. Returns false when the configuration could not be
/// changed byte-exactly (the manual edit was shown).
fn apply(p: &Paths, io: &mut Io, a: &Answers) -> Result<bool, String> {
    // The values kept and checked are those under the lock, not those asked over.
    let held = p.lock_config()?;
    let atlas = &super::atlas_value(p);
    check_order(atlas, a)?;
    let e = edits(atlas, a);
    let label = format!("orchestrate: orchestrated mode on ({})", summary(atlas, a));
    let manual = format!("set {}", e.iter().map(|(k, v)| format!("{} to {}", k.join("."), v.as_ref().map_or("nothing".into(), Value::to_string))).collect::<Vec<_>>().join(", "));
    let w = edit_config_many(&held, io, &e, &label, &manual, None)?;
    drop(held);
    mark_offered(p, std::iter::once(ID));
    if !matches!(w, Written::Refused) {
        say!(io, "[orchestrate] on: {}. `atlas settings orchestrate` asks again; `atlas settings off orchestrate` turns the caps off.", summary(atlas, a));
    }
    Ok(!matches!(w, Written::Refused))
}

/// Writes the no: records it, and turns the caps off when they are on.
fn decline(p: &Paths, io: &mut Io) -> Result<bool, String> {
    let marked = mark_offered(p, std::iter::once(ID));
    let held = p.lock_config()?;
    if get_path(&super::atlas_value(p), &["orchestrate", "enabled"]) == Some(&Value::Bool(true)) {
        let w = edit_config_many(&held, io, &[(&["orchestrate", "enabled"], Some(Value::Bool(false)))], "orchestrate: orchestrated mode off", "set orchestrate.enabled to false", None)?;
        return Ok(!matches!(w, Written::Refused));
    }
    if !marked {
        return Err("orchestrate: could not record the answer in the offer state file, so the question will return".into());
    }
    say!(io, "[orchestrate] not turned on; nothing written. `atlas settings orchestrate` asks again.");
    Ok(true)
}

/// Reads one answer line; `None` at the end of input.
fn read(io: &mut Io) -> Option<String> {
    let _ = io.out.flush();
    let mut line = String::new();
    match io.input.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// Lists the options and how to answer, for a run with no terminal.
pub(super) fn describe(io: &mut Io, atlas: &Value) {
    say!(io, "{ABOUT}");
    say!(io, "  Options (the value in brackets is what applies now or by default):");
    for (i, o) in OPTS.iter().enumerate() {
        say!(io, "    {} {} [{}]", o.flag, o.question, default_label(atlas, o, i));
    }
}

/// The terminal question: yes or no, then each option (Enter keeps the
/// value in brackets; a value that fails its check is asked again, twice
/// at most). The handoff point and limit are skipped when context limits
/// are off. Nothing is written on no or at the end of input.
///
/// # Errors
///
/// A message when the configuration cannot be read or written.
pub fn prompt(p: &Paths, io: &mut Io, items: &[Item], atlas: &Value) -> Result<bool, String> {
    if p.atlas_config.is_none() {
        say!(io, "\n[orchestrate] needs atlas's configuration file, and none is in use (--vault flags only)");
        return Ok(true);
    }
    say!(io, "{ABOUT}");
    let on = get_path(atlas, &["orchestrate", "enabled"]) == Some(&Value::Bool(true));
    let _ = write!(io.out, "  Work in orchestrated mode? [{}] ", if on { "Y/n" } else { "y/N" });
    let Some(yes) = read(io) else {
        say!(io, "\n  no answer; nothing written");
        return Ok(true);
    };
    let yes = match yes.trim().to_ascii_lowercase().as_str() {
        "" => on,
        "y" | "yes" => true,
        _ => false,
    };
    if !yes {
        return decline(p, io);
    }
    say!(io, "  Enter keeps the value in brackets.");
    let mut a: Answers = Default::default();
    for (i, o) in OPTS.iter().enumerate() {
        let off = matches!(a[3].clone().or_else(|| in_effect(atlas, &OPTS[3])), Some(Value::String(ref m)) if m == "off");
        if off && (i == HANDOFF || i == LIMIT) {
            continue;
        }
        for attempt in 0..3 {
            let _ = write!(io.out, "  {} [{}]: ", o.question, default_label(atlas, o, i));
            let Some(line) = read(io) else { break };
            if line.trim().is_empty() {
                break;
            }
            let checked = parse(items, o, &line).and_then(|v| {
                let mut t = a.clone();
                t[i] = Some(v.clone());
                check_order(atlas, &t).map(|()| v)
            });
            match checked {
                Ok(v) => {
                    a[i] = Some(v);
                    break;
                }
                Err(e) if attempt < 2 => say!(io, "  {e}"),
                Err(e) => say!(io, "  {e}; kept [{}]", default_label(atlas, o, i)),
            }
        }
    }
    apply(p, io, &a)
}

/// `atlas settings orchestrate ARGS` (see the module documentation).
///
/// # Errors
///
/// A message for a bad flag or value, or an unwritable configuration.
pub fn command(p: &Paths, io: &mut Io, items: &[Item], atlas: &Value, args: &[String]) -> Result<ExitCode, String> {
    let (mut yes, mut no) = (false, false);
    let mut a: Answers = Default::default();
    let mut it = args.iter();
    while let Some(w) = it.next() {
        match w.as_str() {
            "--yes" => yes = true,
            "--no" => no = true,
            "-h" | "--help" => {
                say!(io, "usage: {USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            f => {
                let i = OPTS.iter().position(|o| o.flag == f).ok_or_else(|| format!("orchestrate: unknown argument {f:?}\nusage: {USAGE}"))?;
                let raw = it.next().ok_or_else(|| format!("{f} needs a value"))?;
                a[i] = Some(parse(items, &OPTS[i], raw)?);
            }
        }
    }
    let flags = a.iter().any(Option::is_some);
    if yes && no {
        return Err("orchestrate: --yes and --no together".into());
    }
    if (flags || yes) && no {
        return Err("orchestrate: --no takes no options".into());
    }
    if flags && !yes {
        return Err(format!("orchestrate: add --yes to apply the options\nusage: {USAGE}"));
    }
    let ok = if yes {
        apply(p, io, &a)?
    } else if no {
        decline(p, io)?
    } else if io.interactive {
        prompt(p, io, items, atlas)?
    } else {
        describe(io, atlas);
        say!(io, "\nNothing went wrong. Nothing was changed because there is no terminal here to answer in. To choose, run `atlas settings orchestrate` in a terminal, or apply answers with `{USAGE}`.");
        true
    };
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// The SessionStart line asking, through Claude, whether the user wants
/// orchestrated mode: until answered (`--yes` or `--no` records it), never when
/// the switch is already set in the configuration or there is no configuration
/// file to write.
pub fn orchestrate_line(p: &Paths) -> Option<String> {
    let cfg = p.atlas_config.as_deref()?;
    if offered(p).iter().any(|x| x == ID) {
        return None;
    }
    let v = super::load(cfg).ok()?.value;
    if get_path(&v, &["orchestrate", "enabled"]).is_some() || !super::can_record(p) {
        return None;
    }
    Some(format!(
        "atlas: orchestrated mode is offered (caps on how many agents of each kind run at once; context limits that hand agents off). In your first reply, whatever the task, ask the user whether to work in orchestrated mode. On yes, ask its options as multiple choice: builders, reviewers and research agents at once (default 2, 2, 1); context limits enforced (default), advisory or off; the handoff point and hard limit (default {}k and {}k on a 1M window, scaled to the window); after a handoff a fresh agent auto (default) or ask. Then run `atlas settings orchestrate --yes --builders N --reviewers N --research N --mode MODE --fresh auto|ask`, adding `--handoff N --limit N` only for numbers the user chose. On no run `atlas settings orchestrate --no`. With no answer, or no user to ask, run nothing: atlas asks again next session.",
        DEFAULT_HANDOFF / 1000,
        DEFAULT_LIMIT / 1000
    ))
}
