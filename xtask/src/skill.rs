//! `cargo xtask skill [--check]`: the bundled atlas skill's generated
//! reference, `skills/atlas/reference.md`.
//!
//! The skill is two files. `SKILL.md` is written by hand: the habits and
//! the workflow. `reference.md` is generated here, never by hand, from the
//! code the server itself answers from: every journal entry kind
//! (`atlas::memory::ENTRIES`), every `atlas queue` command (its usage line),
//! every configuration key (the reader `tests/docs.rs` uses), and the help
//! text the server sends as `atlas://help`, which names every tool, op and
//! argument from the tool schemas. Without `--check` it writes the file;
//! with it, it fails when the file differs.
//!
//! The tests below fail when the file drifts from the code (the fix is
//! always to run `cargo xtask skill`), when a name the hand-written prose
//! or the server instructions put in backticks no longer exists, and when
//! the instructions' habits paragraph passes its token budget.
//!
//! The names checked in a backticked span ([`Known::unknown`]): a tool and
//! its `k=v` arguments and `op`; an `atlas` subcommand or flag (`atlas
//! settings` and `atlas queue` commands with their flags, from their usage
//! lines); a `cargo xtask` command and its flags (xtask's usage); a `word:`
//! meta key (one the queue's or the review gate's reader takes) or a
//! `Word:` launch line; a bare lower-case name; a dotted setting key. Any
//! other span (a placeholder, a record key, a file name, a phrase) is not a
//! name and is not checked. A bare word is checked against every tool's
//! arguments together, not the tool it sits beside in the prose: the span
//! has no context to say which.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

type R<T = ()> = Result<T, String>;

/// The hand-written guide, relative to the workspace root.
pub const SKILL: &str = "skills/atlas/SKILL.md";
/// The generated reference, relative to the workspace root.
pub const REFERENCE: &str = "skills/atlas/reference.md";
/// The most real tokens the habits paragraph of the server instructions
/// may take ([`atlas::tools::GUIDE`]).
///
/// Only the paragraph is budgeted. The line before it names each vault and
/// source the user serves, so its length is the user's own configuration:
/// it was sent before the paragraph existed, and a cap on it could only
/// drop names the user chose to serve.
pub const GUIDE_BUDGET: usize = 200;

/// `bpe::tokens`' error on prose at p90 (the prose row of `bpe.rs`'s table:
/// 10.75%): it uses the card's factor, so the estimate is held this far
/// under [`GUIDE_BUDGET`] for the real count to stay within it.
///
/// [`guide_cap`] divides by 1 + this error (180) rather than multiplying by
/// 1 - it (178). That
/// holds, about 198 real tokens against [`GUIDE_BUDGET`]'s 200, only because
/// the card factor overestimates prose: were the estimate to undercount by
/// the full error, the cap would let the paragraph pass its budget.
pub const PROSE_P90: f64 = 0.1075;

/// The most estimated tokens ([`crate::bpe::tokens`]) the paragraph may
/// take: [`GUIDE_BUDGET`] less [`PROSE_P90`]'s headroom, 180.
pub fn guide_cap() -> usize {
    (GUIDE_BUDGET as f64 / (1.0 + PROSE_P90)).floor() as usize
}

/// The workspace root (this crate's parent).
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask sits in the workspace").to_path_buf()
}

/// The `atlas queue` commands, one per alternative of its usage line:
/// `list [--lane LANE]`, `show KEY`, ...
pub fn queue_commands() -> Vec<String> {
    let u = atlas::queue::USAGE;
    let inner = u.strip_prefix("usage: atlas queue [").and_then(|r| r.strip_suffix(']')).unwrap_or_else(|| panic!("the queue usage line changed shape: {u}"));
    alternatives(inner)
}

/// The `atlas settings` commands, one per alternative of its usage line.
pub fn settings_commands() -> Vec<String> {
    let u = atlas::claude_settings::HELP;
    let inner = u.strip_prefix("usage: atlas settings ").unwrap_or_else(|| panic!("the settings usage line changed shape: {u}"));
    alternatives(inner)
}

/// The `cargo xtask` commands, one per line of xtask's usage that starts
/// one (the same command may have several lines).
pub fn xtask_commands() -> Vec<String> {
    crate::USAGE.lines().filter_map(|l| l.trim().trim_start_matches("usage: ").strip_prefix("cargo xtask ")).map(str::to_string).collect()
}

/// The `atlas` binary's subcommands and its flags, read from its source
/// (`src/main.rs`: `SUBCOMMANDS` and each `USAGE` line), since a binary's
/// items cannot be imported.
fn atlas_cli(root: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
    let src = read(&root.join("src/main.rs")).unwrap_or_else(|e| panic!("{e}"));
    // rustfmt may move the array to the next line, so read to the `]`.
    let subs: BTreeSet<String> = src
        .split_once("const SUBCOMMANDS: &[&str] =")
        .and_then(|(_, rest)| rest.split_once('['))
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(list, _)| list)
        .unwrap_or_else(|| panic!("src/main.rs no longer has `const SUBCOMMANDS: &[&str] = &[...]`"))
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    let usage: Vec<&str> = src.lines().filter(|l| l.trim().starts_with("const USAGE: &str")).collect();
    assert!(!usage.is_empty(), "src/main.rs no longer has `const USAGE: &str`");
    (subs, usage.iter().copied().flat_map(flags).collect())
}

/// The `--flag` words of a usage text.
fn flags(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).filter(|w| w.len() > 2 && w.starts_with("--")).map(str::to_string).collect()
}

/// A usage text's alternatives, parted at ` | ` outside brackets and
/// parentheses.
fn alternatives(inner: &str) -> Vec<String> {
    let (mut out, mut depth, mut cur) = (Vec::new(), 0i32, String::new());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth -= 1,
            // ` | ` parts the commands; `FILE|-` is one argument.
            '|' if depth == 0 && cur.ends_with(' ') && chars.peek() == Some(&' ') => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur.trim().to_string());
    out
}

/// The subcommand words of [`queue_commands`].
pub fn queue_words() -> Vec<String> {
    queue_commands().iter().map(|c| c.split_whitespace().next().unwrap_or("").to_string()).collect()
}

/// Every configuration key the code accepts.
pub fn settings(root: &Path) -> BTreeSet<String> {
    crate::config_keys::all(&root.join("src"))
}

/// The help text with every heading one level down, so it sits under the
/// reference's own title.
fn help_section() -> String {
    let mut out = String::new();
    for l in atlas::tools::help().lines() {
        if l.starts_with('#') {
            out.push('#');
        }
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// The generated reference, as `reference.md` must hold it.
pub fn reference(root: &Path) -> String {
    let mut out = String::from(
        "# atlas reference\n\n\
         <!-- Generated by `cargo xtask skill` from the code; do not edit. A test fails when it drifts. -->\n\n\
         Everything atlas takes, generated from the code the server answers from. The habits and the workflow are \
         in `SKILL.md`. The tools section is the help text the server sends as `atlas://help` (also `note op=help`).\n\n\
         ## Journal entry kinds\n\n",
    );
    for e in atlas::memory::ENTRIES {
        out.push_str(&format!("- `{e}`\n"));
    }
    out.push_str("\n## Queue commands\n\n");
    for c in queue_commands() {
        out.push_str(&format!("- `atlas queue {c}`\n"));
    }
    out.push_str("\n## Settings\n\nEvery key of `atlas.json` (and of a project file); the wiki page Configuration-reference says what each one does.\n\n");
    for k in settings(root) {
        out.push_str(&format!("- `{k}`\n"));
    }
    out.push('\n');
    out.push_str(&help_section());
    out
}

fn read(p: &Path) -> R<String> {
    std::fs::read_to_string(p).map(|t| t.replace("\r\n", "\n")).map_err(|e| format!("{}: {e}", p.display()))
}

/// `cargo xtask skill [--check]`.
pub fn run(args: Vec<String>) -> R {
    let check = match args.as_slice() {
        [] => false,
        [a] if a == "--check" => true,
        _ => return Err("usage: cargo xtask skill [--check]".into()),
    };
    let root = root();
    let path = root.join(REFERENCE);
    let want = reference(&root);
    let have = read(&path).unwrap_or_default();
    if have == want {
        println!("{REFERENCE} is current");
    } else if check {
        return Err(format!("{REFERENCE} differs from the code; run `cargo xtask skill`"));
    } else {
        std::fs::write(&path, want).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("wrote {REFERENCE}");
    }
    // The hand-written parts are not generated: report what they name that
    // the code no longer has, and the instructions' budget.
    let bad = stale_names(&root)?;
    if !bad.is_empty() {
        return Err(format!("names that no longer exist (update the prose):\n{}", bad.join("\n")));
    }
    let (t, cap) = (crate::bpe::tokens(atlas::tools::GUIDE), guide_cap());
    if t > cap {
        return Err(format!("the instructions' guide (atlas::tools::GUIDE) is {t} estimated tokens; the cap is {cap} ({GUIDE_BUDGET} real less the estimate's p90 error on prose)"));
    }
    if !names_style(&read(&root.join(SKILL))?) {
        return Err(format!("{SKILL} does not carry the style rule (atlas::tools::STYLE) word for word"));
    }
    println!("the instructions' guide is {t} of {cap} estimated tokens ({GUIDE_BUDGET} real); the prose names exist; {SKILL} carries the style rule");
    Ok(())
}

/// Whether `text` holds the style rule ([`atlas::tools::STYLE`]) word for
/// word, its whitespace and line breaks aside.
pub fn names_style(text: &str) -> bool {
    let flat = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    flat(text).contains(&flat(atlas::tools::STYLE))
}

/// What `SKILL.md` and the server instructions put in backticks that the
/// code does not have, one line each.
pub fn stale_names(root: &Path) -> R<Vec<String>> {
    let known = Known::new(root);
    let mut bad = Vec::new();
    for (what, text) in [(SKILL, read(&root.join(SKILL))?), ("atlas::tools::GUIDE", atlas::tools::GUIDE.to_string())] {
        bad.extend(stale_in(&known, what, &text)?);
    }
    Ok(bad)
}

/// [`stale_names`] for one Markdown `text`, named `what` in each line.
pub fn stale_in(known: &Known, what: &str, text: &str) -> R<Vec<String>> {
    let spans = code_spans(text).map_err(|e| format!("{what}: {e}"))?;
    let mut bad: Vec<String> = spans.iter().filter_map(|s| known.unknown(s)).map(|e| format!("{what}: {e}")).collect();
    // A fenced launch-line example is no span: it is read as a brief's
    // first lines are.
    for b in fenced_blocks(text).iter().filter(|b| b.lines().any(|l| l.trim_start().starts_with("Kind:"))) {
        if let Some(e) = launch_problem(b) {
            bad.push(format!("{what}: the fenced launch lines {e}:\n{b}"));
        }
    }
    Ok(bad)
}

/// Why the launch lines `block` do not read as a brief's (`Kind:`,
/// `Lane:`, `Priority:` and `Uses:`; `Repo:` and `Branch:` are optional),
/// or `None`.
fn launch_problem(block: &str) -> Option<String> {
    let d = atlas::lanes::declared(block);
    let mut lack = Vec::new();
    if atlas::orchestrate::brief_kind(block).is_none() {
        lack.push("Kind:");
    }
    for (line, read) in [("Lane:", d.lane.is_some()), ("Priority:", d.priority.is_some()), ("Uses:", d.uses.is_some())] {
        if !read {
            lack.push(line);
        }
    }
    (!lack.is_empty()).then(|| format!("give no {}", lack.join(", ")))
}

/// The run of three or more backticks or tildes `line` starts with after
/// its indent: the character, the run's length, the indent and the rest.
fn fence(line: &str) -> Option<(char, usize, usize, &str)> {
    let t = line.trim_start();
    let c = t.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let n = t.chars().take_while(|x| *x == c).count();
    let rest = &t[n..];
    // A backtick fence's info string has no backtick (CommonMark).
    (n >= 3 && !(c == '`' && rest.contains('`'))).then_some((c, n, line.len() - t.len(), rest))
}

/// Markdown `text` parted into its prose lines (each with its line number)
/// and its fenced blocks (each without its fence lines, the opening fence's
/// indent taken off). A fence opens on three or more backticks or tildes and
/// closes on at least as many of the same character with nothing after
/// them; one left open runs to the end.
fn split_fences(text: &str) -> (Vec<(usize, &str)>, Vec<String>) {
    let (mut prose, mut blocks) = (Vec::new(), Vec::new());
    // The open fence: its character, length, indent and body so far.
    let mut open: Option<(char, usize, usize, String)> = None;
    for (i, l) in text.lines().enumerate() {
        match open.as_mut() {
            None => match fence(l) {
                Some((c, n, indent, _)) => open = Some((c, n, indent, String::new())),
                None => prose.push((i + 1, l)),
            },
            Some((c, n, indent, body)) => {
                if fence(l).is_some_and(|(c2, n2, _, rest)| c2 == *c && n2 >= *n && rest.trim().is_empty()) {
                    blocks.push(std::mem::take(body));
                    open = None;
                } else {
                    let cut = l.bytes().take_while(|b| matches!(b, b' ' | b'\t')).count();
                    body.push_str(&l[cut.min(*indent)..]);
                    body.push('\n');
                }
            }
        }
    }
    blocks.extend(open.map(|o| o.3));
    (prose, blocks)
}

/// The fenced blocks of Markdown `text`, each without its fence lines.
pub fn fenced_blocks(text: &str) -> Vec<String> {
    split_fences(text).1
}

/// The names the code knows, for checking hand-written prose.
pub struct Known {
    /// Tool name to the arguments it takes (a listed tool: its own and
    /// every argument of each full tool its ops run).
    args: std::collections::BTreeMap<String, BTreeSet<String>>,
    /// Tool name to its ops (`note` also has `help`).
    ops: std::collections::BTreeMap<String, BTreeSet<String>>,
    /// Every name a single-word span may be.
    words: BTreeSet<String>,
    /// Every configuration key, and every section above one.
    settings: BTreeSet<String>,
    queue: Vec<String>,
    settings_cmds: Vec<String>,
    xtask: Vec<String>,
    /// The `atlas` binary's subcommands and flags.
    atlas_subs: BTreeSet<String>,
    atlas_flags: BTreeSet<String>,
}

/// A review's meta lines as the merge gate reads them; each is required
/// (`atlas::reviews::Review::parse`), so a key is the gate's when the
/// review stops reading without its line.
const REVIEW_SAMPLE: [&str; 4] = ["role: cost", "pr: 1", "head: abcdef0", "verdict: pass"];

/// Meta keys atlas files as written and no reader takes: accepted when
/// `atlas://help` names them (`report:`).
const KEPT_AS_WRITTEN: &[&str] = &["report"];

fn props(t: &atlas::json::Value) -> BTreeSet<String> {
    t["inputSchema"]["properties"].as_object().map(|p| p.iter().map(|(k, _)| k.to_string()).collect()).unwrap_or_default()
}

impl Known {
    pub fn new(root: &Path) -> Known {
        use std::collections::BTreeMap;
        let mut args: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut ops: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let full: BTreeMap<String, BTreeSet<String>> = atlas::tools::full_tool_list().iter().map(|t| (t["name"].as_str().unwrap_or("").to_string(), props(t))).collect();
        for t in atlas::tools::tool_list() {
            args.insert(t["name"].as_str().unwrap_or("").to_string(), props(&t));
        }
        for (tool, op, runs) in atlas::tools::ops() {
            ops.entry(tool.to_string()).or_default().insert(op.to_string());
            let a = full.get(runs).cloned().unwrap_or_default();
            args.entry(tool.to_string()).or_default().extend(a);
        }
        ops.entry("note".into()).or_default().insert("help".into());
        for (name, a) in &full {
            args.entry(name.clone()).or_default().extend(a.iter().cloned());
        }
        let mut settings = BTreeSet::new();
        for k in self::settings(root) {
            let parts: Vec<&str> = k.split('.').collect();
            for i in 1..=parts.len() {
                settings.insert(parts[..i].join("."));
            }
        }
        let mut words: BTreeSet<String> = args.keys().cloned().collect();
        words.extend(args.values().flatten().cloned());
        words.extend(ops.values().flatten().cloned());
        words.extend(atlas::memory::ENTRIES.iter().map(|e| e.to_string()));
        words.extend(queue_words());
        words.extend(settings.iter().filter(|s| !s.contains('.')).cloned());
        // The skills the plugin bundles, by their own names.
        words.extend(["atlas", "orchestrate"].map(String::from));
        assert!(atlas::reviews::Review::parse(&REVIEW_SAMPLE.join("\n")).is_some(), "the review gate no longer reads {REVIEW_SAMPLE:?}: update the sample");
        let (atlas_subs, atlas_flags) = atlas_cli(root);
        Known { args, ops, words, settings, queue: queue_commands(), settings_cmds: settings_commands(), xtask: xtask_commands(), atlas_subs, atlas_flags }
    }

    /// Whether `key` (a `key:` meta line) is read: by the queue's reader
    /// (which keeps an unknown line as written), by the review gate (whose
    /// lines are each required), or is kept as written and named by help.
    fn meta_key(&self, key: &str) -> bool {
        let queue = !matches!(atlas::queue::planned_from_meta("L", "t", "", &format!("{key}: 1")), Ok(p) if !p.extra.is_empty());
        let review = REVIEW_SAMPLE.iter().any(|l| l.split(':').next() == Some(key))
            && atlas::reviews::Review::parse(&REVIEW_SAMPLE.iter().filter(|l| l.split(':').next() != Some(key)).copied().collect::<Vec<_>>().join("\n")).is_none();
        let kept = KEPT_AS_WRITTEN.contains(&key) && atlas::tools::help().contains(&format!("`{key}:`"));
        queue || review || kept
    }

    /// Whether `key` (a `Key:` launch line) is read from a brief.
    fn launch_key(key: &str) -> bool {
        let line = |v: &str| format!("{key}: {v}");
        let d = atlas::lanes::declared(&format!("{}\n{}\n{}", line("X-1"), line("urgent"), line("compile")));
        atlas::orchestrate::brief_kind(&line("builder")).is_some()
            || atlas::queue::queue_line(&line("journal:Q:0001")).is_some()
            || d.lane.is_some()
            || d.priority.is_some()
            || d.uses.is_some()
            || d.repo.is_some()
            || d.branch.is_some()
    }

    /// Why the command `span` (`<what> <sub> ...`) names a subcommand or a
    /// `--flag` that `alts` (its usage's alternatives) lack.
    ///
    /// Each word after the first, up to the first flag, is a subcommand
    /// when the alternatives so far name subcommands at its place and no
    /// argument (`release assets` and `release sign`, but `set ID`); a
    /// placeholder (upper case, `<..>`) ends the walk.
    fn command(span: &str, what: &str, rest: &[&str], alts: &[String]) -> Option<String> {
        let sub = *rest.first()?;
        let mut of: Vec<Vec<&str>> =
            alts.iter().map(|a| a.split_whitespace().collect::<Vec<_>>()).filter(|a| a.first() == Some(&sub)).collect();
        if of.is_empty() {
            return Some(format!("`{span}`: {what} has no command {sub}"));
        }
        let is_sub = |t: &str| {
            t.starts_with(|c: char| c.is_ascii_lowercase()) && t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        };
        let mut path = vec![sub];
        for (at, w) in rest.iter().copied().enumerate().skip(1) {
            if w.starts_with('-') || w.starts_with('<') || w.chars().any(|c| c.is_ascii_uppercase()) {
                break;
            }
            let tokens: Vec<&str> = of.iter().filter_map(|a| a.get(at).map(|t| t.trim_start_matches(['[', '(']))).collect();
            if tokens.iter().any(|t| !t.starts_with('-') && !is_sub(t)) || !tokens.iter().any(|t| is_sub(t)) {
                break;
            }
            if !tokens.contains(&w) {
                return Some(format!("`{span}`: {what} {} has no command {w}", path.join(" ")));
            }
            of.retain(|a| a.get(at).map(|t| t.trim_start_matches(['[', '('])) == Some(w));
            path.push(w);
        }
        let of: Vec<String> = of.iter().map(|a| a.join(" ")).collect();
        let flag = rest[1..].iter().filter(|w| w.starts_with("--")).map(|w| w.split('=').next().unwrap_or(w)).find(|f| !of.iter().any(|a| flags(a).contains(*f)))?;
        Some(format!("`{span}`: {what} {} takes no flag {flag}", path.join(" ")))
    }

    /// Why `span` (one inline code span) names something the code does not
    /// have, or `None`. What is checked is in the module doc.
    pub fn unknown(&self, span: &str) -> Option<String> {
        let words: Vec<&str> = span.split_whitespace().collect();
        let first = *words.first()?;
        if let Some(key) = first.strip_suffix(':').filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
            let known = if key.starts_with(|c: char| c.is_ascii_uppercase()) { Known::launch_key(key) } else { self.meta_key(key) };
            return (!known).then(|| format!("`{span}`: no meta or launch line reader takes {key}:"));
        }
        match (first, words.get(1).copied()) {
            ("atlas", Some("queue")) => return Known::command(span, "atlas queue", &words[2..], &self.queue),
            ("atlas", Some("settings")) => return Known::command(span, "atlas settings", &words[2..], &self.settings_cmds),
            ("cargo", Some("xtask")) => return Known::command(span, "cargo xtask", &words[2..], &self.xtask),
            ("atlas", Some(sub)) => {
                if !sub.starts_with("--") && !self.atlas_subs.contains(sub) {
                    return Some(format!("`{span}`: atlas has no subcommand {sub}"));
                }
                let flag = words[1..].iter().filter(|w| w.starts_with("--")).map(|w| w.split('=').next().unwrap_or(w)).find(|f| !self.atlas_flags.contains(*f))?;
                return Some(format!("`{span}`: atlas takes no flag {flag}"));
            }
            _ => {}
        }
        if let Some(args) = self.args.get(first) {
            for w in &words[1..] {
                let Some((k, v)) = w.split_once('=') else { continue };
                if !args.contains(k) {
                    return Some(format!("`{span}`: {first} takes no argument {k}"));
                }
                if k == "op" && !v.starts_with('<') && !self.ops.get(first).is_some_and(|o| o.contains(v)) {
                    return Some(format!("`{span}`: {first} has no op {v}"));
                }
            }
            return None;
        }
        if words.len() > 1 {
            return None;
        }
        let bare = |s: &str| !s.is_empty() && s.starts_with(|c: char| c.is_ascii_lowercase()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if bare(span) {
            return (!self.words.contains(span)).then(|| format!("`{span}`: no tool, op, argument, entry kind, queue command or setting has that name"));
        }
        let file = [".md", ".json", ".rs", ".toml"].iter().any(|x| span.ends_with(x));
        if !file && span.contains('.') && span.split('.').all(|p| bare(p.trim_end_matches("[]"))) {
            return (!self.settings.contains(span)).then(|| format!("`{span}`: no such setting"));
        }
        None
    }
}

/// The inline code spans of Markdown `text`, fenced blocks (backtick or
/// tilde, [`split_fences`]) left out, each with its whitespace runs made one
/// space. A span opens on a run of backticks and closes on the next run of
/// the same length, so ``` `` a`b `` ``` is the one span ``a`b``. Not a full
/// CommonMark reader: what it does not read, it refuses.
///
/// # Errors
///
/// A run of backticks no run of its length closes: a stray one would pair
/// every later span with the prose between spans and switch the check off.
/// A backslash before a backtick outside a span: an escaped backtick would
/// pair the same way, so it is refused with the way to write it instead.
pub fn code_spans(text: &str) -> R<Vec<String>> {
    let (lines, _) = split_fences(text);
    let (mut prose, mut starts) = (String::new(), Vec::new());
    for (n, l) in lines {
        starts.push((prose.len(), n));
        prose.push_str(l);
        prose.push('\n');
    }
    let line = |at: usize| starts.iter().rev().find(|(s, _)| *s <= at).map_or(0, |(_, n)| *n);
    let b = prose.as_bytes();
    let run = |at: usize| b[at..].iter().take_while(|c| **c == b'`').count();
    let (mut spans, mut i) = (Vec::new(), 0);
    while i < b.len() {
        match b[i] {
            b'\\' if b.get(i + 1) == Some(&b'`') => {
                return Err(format!(
                    "line {}: an escaped backtick (\\`) outside a code span is not read; put the backtick inside a double-backtick span instead (`` a`b ``)",
                    line(i)
                ));
            }
            // A backslash escapes the next character (`\\` is one backslash).
            b'\\' => i += 2,
            b'`' => {
                let n = run(i);
                let mut j = i + n;
                while j < b.len() && !(b[j] == b'`' && run(j) == n) {
                    j += if b[j] == b'`' { run(j) } else { 1 };
                }
                if j >= b.len() {
                    return Err(format!(
                        "line {}: a run of {n} backtick(s) that no run of {n} closes: one is stray, and the spans after it would not be checked",
                        line(i)
                    ));
                }
                spans.push(prose[i + n..j].split_whitespace().collect::<Vec<_>>().join(" "));
                i = j + n;
            }
            _ => i += 1,
        }
    }
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill() -> String {
        read(&root().join(SKILL)).unwrap()
    }

    #[test]
    fn the_reference_is_generated_from_the_code() {
        let root = root();
        let have = read(&root.join(REFERENCE)).unwrap_or_default();
        assert!(have == reference(&root), "{REFERENCE} differs from the code: run `cargo xtask skill` and commit it");
    }

    /// Belt and braces beside the equality: whatever the generator does, the
    /// reference names every tool, op, argument, entry kind, queue command
    /// and setting, and holds the help text whole.
    #[test]
    fn the_reference_names_everything_the_code_takes() {
        let root = root();
        let r = reference(&root);
        let mut missing = Vec::new();
        for t in atlas::tools::tool_list().iter().chain(atlas::tools::full_tool_list().iter()) {
            let name = t["name"].as_str().unwrap();
            if !r.contains(&format!("`{name}`")) && !r.contains(&format!("### {name}\n")) {
                missing.push(format!("tool {name}"));
            }
            for a in props(t) {
                // A merged tool's `op` is named by its ops (`op `x` runs ...`).
                let op_lines = a == "op" && r.contains(&format!("- `{name}`: ")) && atlas::tools::ops().any(|(t, _, _)| t == name);
                if !op_lines && !r.contains(&format!("`{a}`")) {
                    missing.push(format!("argument {name}.{a}"));
                }
            }
        }
        for (tool, op, _) in atlas::tools::ops() {
            if !r.contains(&format!("op `{op}`")) {
                missing.push(format!("op {tool} {op}"));
            }
        }
        for e in atlas::memory::ENTRIES {
            if !r.contains(&format!("- `{e}`\n")) {
                missing.push(format!("entry {e}"));
            }
        }
        for q in queue_words() {
            if !r.contains(&format!("`atlas queue {q}")) {
                missing.push(format!("queue {q}"));
            }
        }
        for k in settings(&root) {
            if !r.contains(&format!("- `{k}`\n")) {
                missing.push(format!("setting {k}"));
            }
        }
        assert!(missing.is_empty(), "the reference leaves out {missing:?}");
        let help = atlas::tools::help();
        assert!(r.contains(&help_section()), "the reference holds atlas://help whole");
        let lost: Vec<&str> = help.lines().filter(|l| !l.starts_with('#') && !r.contains(l)).collect();
        assert!(lost.is_empty(), "lines of atlas://help the reference lacks: {lost:?}");
        for k in ["cap.main_session", "compact.clear_floor", "recall.subagent_context", "memory.transcripts"] {
            assert!(r.contains(&format!("`{k}`")), "the settings reader no longer sees {k}");
        }
    }

    #[test]
    fn every_name_the_guide_and_the_instructions_use_exists() {
        let bad = stale_names(&root()).unwrap();
        assert!(bad.is_empty(), "names that no longer exist (update the prose):\n{}", bad.join("\n"));
    }

    /// The checker catches what it is for: a removed tool, op, argument,
    /// queue command and setting.
    #[test]
    fn the_name_check_catches_stale_names() {
        let known = Known::new(&root());
        for ok in ["recall lane=<id>", "note op=help", "journal", "amend", "atlas queue met KEY CONDITION", "compact.clear_floor", "orchestrate", "Kind: builder", "journal:LANE:NNNN", "reference.md"] {
            assert_eq!(known.unknown(ok), None, "{ok}");
        }
        for stale in ["recall lanes=<id>", "note op=helpme", "journals", "atlas queue start KEY", "compact.clear_ceiling", "remember_all"] {
            assert!(known.unknown(stale).is_some(), "{stale} passed");
        }
    }

    /// The commands, flags and meta keys the prose names, each beside a
    /// rename of it: the name passes, the rename is caught.
    #[test]
    fn the_name_check_catches_renamed_commands_flags_and_keys() {
        let known = Known::new(&root());
        let pairs = [
            ("atlas settings auto-compact", "atlas settings compact-at"),
            ("atlas settings auto-compact --yes", "atlas settings auto-compact --sure"),
            ("atlas --rescan", "atlas --reindex"),
            ("atlas queue plan --lane L --title T", "atlas queue plan --lane L --name T"),
            ("atlas queue next --transcript PATH", "atlas queue next --session PATH"),
            ("cargo xtask memory-eval", "cargo xtask memory-evals"),
            ("cargo xtask rules-eval", "cargo xtask ruling-eval"),
            ("cargo xtask skill --check", "cargo xtask skill --verify"),
            ("kind:", "kinds:"),
            ("priority:", "prio:"),
            ("uses:", "use:"),
            ("after:", "before:"),
            ("order:", "rank:"),
            ("role:", "roles:"),
            ("pr:", "pull:"),
            ("head:", "sha:"),
            ("verdict:", "outcome:"),
            ("report:", "reports:"),
            ("Kind:", "Kinds:"),
            ("Lane:", "Lanes:"),
            ("Queue:", "Queued:"),
            ("Uses:", "Using:"),
            ("Priority:", "Prio:"),
            ("Kind: builder", "Type: builder"),
        ];
        for (ok, renamed) in pairs {
            assert_eq!(known.unknown(ok), None, "{ok}");
            assert!(known.unknown(renamed).is_some(), "{renamed} passed");
        }
    }

    /// A stray backtick would pair every later span with the prose between
    /// spans and switch the check off in silence; it is refused instead.
    #[test]
    fn a_stray_backtick_fails_loudly() {
        assert!(code_spans("a `b` c ` d `e`").is_err());
        assert!(code_spans("a `b`\n```text\nx ` y\n```\nc `d`").is_ok(), "a backtick in a fence does not count");
    }

    /// The launch-line example in SKILL.md is fenced, so no span check sees
    /// it: it is read as a brief's first lines instead, and a misspelt line
    /// in it is caught.
    #[test]
    fn the_fenced_launch_lines_read_as_launch_lines() {
        let s = skill();
        let blocks: Vec<String> =
            fenced_blocks(&s).into_iter().filter(|b| b.lines().any(|l| l.trim_start().starts_with("Kind:"))).collect();
        assert_eq!(blocks.len(), 1, "one fenced block holds the launch lines: {blocks:?}");
        let d = atlas::lanes::declared(&blocks[0]);
        assert!(d.lane.is_some() && d.priority.is_some() && d.uses.is_some(), "{d:?} from {}", blocks[0]);
        assert!(atlas::orchestrate::brief_kind(&blocks[0]).is_some(), "no kind in {}", blocks[0]);
        let known = Known::new(&root());
        assert_eq!(stale_in(&known, SKILL, &s).unwrap(), Vec::<String>::new());
        let typo = s.replace("  Kind: builder\n", "  Kind: buildr\n");
        assert_ne!(typo, s, "SKILL.md has the fenced `Kind: builder` line");
        assert!(!stale_in(&known, SKILL, &typo).unwrap().is_empty(), "a fenced `Kind: buildr` passed");
    }

    /// A `~~~` fence is a fence, a double-backtick span is one span (and its
    /// names are checked), and an escaped backtick is refused with the way
    /// to write it.
    #[test]
    fn tilde_fences_and_double_backtick_spans_are_read() {
        let known = Known::new(&root());
        let text = "a `journal`\n~~~text\nx ` y\n```\n~~~\nb `journals`\n";
        assert_eq!(code_spans(text).unwrap(), ["journal", "journals"]);
        assert!(!stale_in(&known, "t", text).unwrap().is_empty(), "a stale name after a tilde fence passed");
        let text = "a ``recall lanes=`x` `` b\n";
        assert_eq!(code_spans(text).unwrap(), ["recall lanes=`x`"]);
        assert!(!stale_in(&known, "t", text).unwrap().is_empty(), "a stale name in a double-backtick span passed");
        let e = code_spans("a \\` b `journal`\n").unwrap_err();
        assert!(e.contains("escaped backtick") && e.contains("double-backtick span"), "{e}");
    }

    /// A parenthesised group's ` | ` stays inside one alternative.
    #[test]
    fn a_bar_inside_parentheses_does_not_split_a_usage() {
        assert_eq!(alternatives("a [--x | --y] (DIR | --yes) | b"), ["a [--x | --y] (DIR | --yes)", "b"]);
        assert!(
            settings_commands().iter().any(|c| c.starts_with("add-project ") && c.ends_with("(DIR | --yes)")),
            "{:?}",
            settings_commands()
        );
    }

    /// The words after a command's first are checked when its usage names
    /// subcommands there; a placeholder argument is not one.
    #[test]
    fn a_second_subcommand_word_is_checked() {
        let known = Known::new(&root());
        for ok in [
            "cargo xtask release assets --dir DIR",
            "cargo xtask release sign v0.6.1",
            "cargo xtask release sign vX.Y.Z",
            "atlas queue release AGENT",
            "atlas settings set ID VALUE",
        ] {
            assert_eq!(known.unknown(ok), None, "{ok}");
        }
        for bad in ["cargo xtask release nosuch", "cargo xtask release asset --dir DIR"] {
            assert!(known.unknown(bad).is_some(), "{bad} passed");
        }
    }

    /// One entry per `atlas queue` command; `FILE|-` stays one argument.
    #[test]
    fn the_queue_commands_split_on_the_usage_line_s_bars() {
        let w = queue_words();
        assert_eq!(w, ["list", "show", "drop", "done", "met", "next", "release", "add", "plan"]);
        assert!(queue_commands().iter().any(|c| c.starts_with("add ") && c.ends_with("FILE|-")), "{:?}", queue_commands());
    }

    #[test]
    fn the_skill_says_when_to_use_it() {
        let s = skill();
        let front = s.strip_prefix("---\n").and_then(|r| r.split_once("\n---\n")).map(|(f, _)| f).expect("front matter");
        assert!(front.lines().any(|l| l == "name: atlas"), "{front}");
        let d = front.lines().find_map(|l| l.strip_prefix("description: ")).expect("a description");
        assert!(d.len() <= 1024, "a skill description is at most 1024 characters: {}", d.len());
        for w in ["orchestrat", "brief", "recall", "decided", "compact"] {
            assert!(d.contains(w), "the description triggers on {w}: {d}");
        }
        assert!(s.contains("reference.md"), "the guide points at the generated reference");
    }

    /// The habits paragraph of the server instructions: within budget with
    /// the estimate's prose error as headroom, and it names each habit and
    /// the skill. (The served line before it is not budgeted: see
    /// [`GUIDE_BUDGET`].)
    #[test]
    fn the_instructions_stay_within_budget_and_name_the_habits() {
        let g = atlas::tools::GUIDE;
        let t = crate::bpe::tokens(g);
        assert_eq!(guide_cap(), 180);
        assert!((100..=guide_cap()).contains(&t), "the instructions' guide is {t} estimated tokens; the cap is {} ({GUIDE_BUDGET} real): {g}", guide_cap());
        for w in ["recall", "journal", "handoff", "queue key", "Lane:", "`atlas` skill", "atlas://help", "before your first journal or handoff", "whenever work is left for later"] {
            assert!(g.contains(w), "the instructions name {w}: {g}");
        }
        assert!(!g.contains('\n'), "one paragraph");
    }

    /// The style rule (#274) is in SKILL.md word for word (its line breaks
    /// aside), and the check `cargo xtask skill` runs says so.
    #[test]
    fn the_skill_carries_the_style_rule() {
        assert!(names_style(&skill()), "SKILL.md carries atlas::tools::STYLE: {}", atlas::tools::STYLE);
        assert!(!names_style("Style for what you write into atlas: anything goes."));
    }

    /// The handoff habit, in the skill and in the rules template the setup
    /// writes: a handoff whenever work is left for later, not only at a stop.
    #[test]
    fn the_handoff_habit_covers_work_left_for_later() {
        let s = skill();
        let item = s.lines().skip_while(|l| !l.starts_with("3. **")).take(3).collect::<Vec<_>>().join(" ");
        assert!(item.contains("whenever work is left for later"), "SKILL.md item 3: {item}");
        let t = atlas::claude_settings::rules_template::RULES_CLAUDE_MD;
        let rule = t.lines().find(|l| l.contains("Write a handoff")).unwrap_or("");
        assert!(rule.contains("whenever work is left for later"), "templates/CLAUDE.md: {rule}");
        assert!(t.is_ascii(), "the template is ASCII");
    }
}
