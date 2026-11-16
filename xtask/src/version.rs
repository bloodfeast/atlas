//! `cargo xtask version`: the next version, from the changelog fragments.
//!
//! ```text
//! cargo xtask version [--dry-run] [--release | --set X.Y.Z]
//! ```
//!
//! Every pull request that changes what users get adds a fragment,
//! `changes/<pr>.md` (more than one as `changes/<pr>-<slug>.md`), or marks a
//! docs, CI or test-only change with an empty `changes/<pr>.none`
//! (changes/README.md has the format). Merged fragments wait in `changes/`
//! until a release is cut. This command then:
//!
//! 1. reads every fragment, and refuses a malformed one, a stray file, a
//!    fragment git does not track, or an empty set (no `.md` fragment:
//!    `.none` markers are not changes);
//! 2. replays the fragments in merge order ([`in_merge_order`]) from
//!    Cargo.toml's version, one step each ([`next_version`]), or with
//!    `--release` raises the release number by hand ([`replay`]), or with
//!    `--set X.Y.Z` takes that version outright ([`target`]);
//! 3. folds the fragments into one new `## vX.Y.Z` section at the top of
//!    CHANGELOG.md, its parts in the header's order, and refuses a section
//!    with no Highlights (some fragment must be `section: highlight`);
//! 4. writes the version into Cargo.toml, Cargo.lock (atlas's own entry) and
//!    `.claude-plugin/plugin.json`, deletes every fragment and marker it read,
//!    and makes one commit of all of it, or puts every file back if any step
//!    fails.
//!
//! `--dry-run` prints the steps, the version and the section and changes
//! nothing. The commit goes through a pull request like any other; `cargo
//! xtask release ship vX.Y.Z` then refuses to start while fragments are
//! still pending ([`refuse_pending`]).
//!
//! The version is Release.Feature.Patch, by the same rule above and below
//! 1.0.0. A `patch` fragment raises the last number; a `feature` fragment
//! raises the middle one and resets the last to 0; a `breaking` fragment
//! raises the first and resets the other two. So 0.5.0 with fifteen patches
//! and then a feature is 0.6.0, and 0.6.0 with patch, patch, feature, patch
//! is 0.7.1. Otherwise the release number goes up only by hand:
//! `--release` makes the next version (X+1).0.0 whatever the fragments say.
//! `--set X.Y.Z` is the one-off override: every pending fragment goes into
//! one section at that version, the replay ignored; it must be strictly above
//! Cargo.toml's version and cannot go with `--release` (first use: v0.6.0
//! for the batch since v0.5.0, the replay counting from there). Neither
//! reads a tag.
//!
//! The dev-pr workflow's check of a pull request's fragment is
//! `scripts/changes-check.sh`, which applies the same format rules; the tests
//! here hold the two to the same verdicts.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::release::{cargo_version, check_section, control_why, nested_heading, output, plugin_version, read_text, setext_underline, stray_control, write, Fence};

type R<T = ()> = Result<T, String>;

/// Usage.
pub const USAGE: &str = "usage: cargo xtask version [--dry-run] [--release | --set X.Y.Z]";

/// Which version a release gets: the fragments' replay, the release number
/// raised by hand (`--release`), or one given outright (`--set X.Y.Z`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum To {
    Replay,
    Release,
    Set(String),
}

/// The fragments' folder, from the repository root.
pub const DIR: &str = "changes";

/// A fragment's `bump:`: which part of Release.Feature.Patch it raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bump {
    /// A change to an existing feature: a fix, performance or correctness.
    Patch,
    /// Feature work.
    Feature,
    /// A change a user must act on: forces a release bump.
    Breaking,
}

impl Bump {
    fn parse(s: &str) -> Option<Bump> {
        match s {
            "patch" => Some(Bump::Patch),
            "feature" => Some(Bump::Feature),
            "breaking" => Some(Bump::Breaking),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Bump::Patch => "patch",
            Bump::Feature => "feature",
            Bump::Breaking => "breaking",
        }
    }
}

/// A fragment's `section:`, in the CHANGELOG's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// `### Highlights`.
    Highlight,
    /// `### Breaking changes`.
    Breaking,
    /// `### Upgrade notes`.
    Upgrade,
    /// `### Features`.
    Feature,
    /// `### Fixes`.
    Fix,
    /// `### Security hardening`.
    Security,
    /// `### Known limits`.
    KnownLimit,
}

/// Every section: its front matter key and its CHANGELOG heading, in order.
pub const SECTIONS: [(Section, &str, &str); 7] = [
    (Section::Highlight, "highlight", "Highlights"),
    (Section::Breaking, "breaking", "Breaking changes"),
    (Section::Upgrade, "upgrade", "Upgrade notes"),
    (Section::Feature, "feature", "Features"),
    (Section::Fix, "fix", "Fixes"),
    (Section::Security, "security", "Security hardening"),
    (Section::KnownLimit, "known-limit", "Known limits"),
];

impl Section {
    fn parse(s: &str) -> Option<Section> {
        SECTIONS.iter().find(|(_, key, _)| *key == s).map(|(section, _, _)| *section)
    }
}

/// A run of ASCII digits as a number.
fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A line's own line ending.
fn eol(l: &str) -> &str {
    if l.ends_with("\r\n") {
        "\r\n"
    } else if l.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

/// A line without its line ending.
fn bare(l: &str) -> &str {
    l.trim_end_matches(['\r', '\n'])
}

fn say(m: &str) {
    println!("version: {m}");
}

const NOTHING: &str = "no changelog fragments in changes/: nothing to release (a .none marker is not a change)";

/// What a file in `changes/` is, by its name.
#[derive(Debug, PartialEq, Eq)]
pub enum Entry {
    /// `<pr>.md` or `<pr>-<slug>.md`.
    Fragment(u64),
    /// `<pr>.none`.
    None(u64),
    /// `README.md`.
    Readme,
}

/// A parsed fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// Its file name in `changes/`.
    pub name: String,
    /// The pull request number its name starts with.
    pub pr: u64,
    /// Its bump.
    pub bump: Bump,
    /// Its section.
    pub section: Section,
    /// Its body, blank lines at both ends trimmed.
    pub body: String,
}

/// What a name in `changes/` is, or why it is none of them.
pub fn entry(name: &str) -> R<Entry> {
    let bad = || format!("changes/{name}: not a fragment name (changes/<pr>.md, changes/<pr>-<slug>.md or changes/<pr>.none; see changes/README.md)");
    let pr_of = |s: &str| number(s).filter(|_| !s.starts_with('0'));
    if name == "README.md" {
        return Ok(Entry::Readme);
    }
    if let Some(stem) = name.strip_suffix(".none") {
        return pr_of(stem).map(Entry::None).ok_or_else(bad);
    }
    let stem = name.strip_suffix(".md").ok_or_else(bad)?;
    let (num, slug) = match stem.split_once('-') {
        Some((n, s)) => (n, Some(s)),
        None => (stem, None),
    };
    let pr = pr_of(num).ok_or_else(bad)?;
    let slug_ok = |s: &str| s.split('-').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    if slug.is_some_and(|s| !slug_ok(s)) {
        return Err(bad());
    }
    Ok(Entry::Fragment(pr))
}

/// Parses a fragment file's bytes: UTF-8 text, then [`parse_fragment`]
/// (scripts/changes-check.sh checks the encoding first too).
pub fn parse_fragment_bytes(name: &str, bytes: &[u8]) -> R<Fragment> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| format!("changes/{name}: not UTF-8 text (at byte {}): save it as UTF-8", e.valid_up_to()))?;
    parse_fragment(name, text)
}

/// Parses a fragment (see changes/README.md). The checks run in the order
/// scripts/changes-check.sh runs them, so the two name the same fault.
pub fn parse_fragment(name: &str, text: &str) -> R<Fragment> {
    let pr = match entry(name)? {
        Entry::Fragment(pr) => pr,
        _ => return Err(format!("changes/{name}: not a fragment name")),
    };
    let err = |why: &str| format!("changes/{name}: {why}");
    if text.starts_with('\u{feff}') {
        return Err(err("starts with a byte order mark (BOM): save it as UTF-8 without one"));
    }
    if let Some(c) = stray_control(text) {
        return Err(err(&control_why(c)));
    }
    let mut lines = text.lines().map(|l| l.strip_suffix('\r').unwrap_or(l));
    if lines.next() != Some("---") {
        return Err(err("the front matter must open with ---"));
    }
    let (mut bump, mut section, mut seen, mut closed) = (None, None, Vec::new(), false);
    for l in lines.by_ref() {
        if l == "---" {
            closed = true;
            break;
        }
        if l.trim().is_empty() {
            continue;
        }
        let (k, v) = l.split_once(':').ok_or_else(|| err(&format!("a front matter line is not key: value: {l}")))?;
        let (k, v) = (k.trim(), v.trim());
        if seen.contains(&k) {
            return Err(err(&format!("duplicate front matter key {k}")));
        }
        seen.push(k);
        match k {
            "bump" => bump = Some(Bump::parse(v).ok_or_else(|| err(&format!("bump must be patch, feature or breaking, not {v}")))?),
            "section" => {
                section = Some(Section::parse(v).ok_or_else(|| {
                    err(&format!("section must be highlight, breaking, upgrade, feature, fix, security or known-limit, not {v}"))
                })?)
            }
            _ => return Err(err(&format!("unknown front matter key {k} (only bump and section)"))),
        }
    }
    let body: Vec<&str> = lines.collect();
    let (mut fence, mut started) = (Fence::default(), false);
    for l in &body {
        let fenced = fence.opaque(l);
        if fence.short() {
            return Err(err(
                "a line inside a fenced block is indented less than the fence that opened it, which ends the block on GitHub: indent every line of the block, its closing fence included, at least as far as its opening fence",
            ));
        }
        if !fenced && l.starts_with('#') {
            return Err(err("a heading in the body: the fold writes the headings"));
        }
        if !fenced && nested_heading(l) {
            return Err(err("a heading after an indent or a list marker, which GitHub renders as a heading: the fold writes the headings"));
        }
        if !fenced && setext_underline(l) {
            return Err(err("a line of only = or -, which GitHub renders as a heading underline under the line above: reword it"));
        }
        if !started {
            if l.trim().is_empty() {
                continue;
            }
            if !l.starts_with("- ") {
                return Err(err("the body must be a list: its first line starts with \"- \""));
            }
            started = true;
        }
        // GitHub reads such a line as HTML, which can hide everything after
        // it (an open comment, `<details>`): comments are refused too.
        if !fenced && l.trim_start_matches([' ', '\t']).starts_with('<') {
            return Err(err("a line starts with < outside a fenced block, which GitHub reads as HTML that can hide the rest of the notes: reword it, or put it in a fenced block"));
        }
    }
    if !closed {
        return Err(err("the front matter has no closing ---"));
    }
    let bump = bump.ok_or_else(|| err("no bump in the front matter"))?;
    let section = section.ok_or_else(|| err("no section in the front matter"))?;
    if section == Section::Breaking && bump != Bump::Breaking {
        return Err(err("section: breaking needs bump: breaking"));
    }
    let start = body.iter().position(|l| !l.trim().is_empty()).ok_or_else(|| err("the body is empty"))?;
    if fence.open() {
        return Err(err("an unclosed fenced block: close it with a fence of the same character, at least as long"));
    }
    let end = body.iter().rposition(|l| !l.trim().is_empty()).map_or(start, |e| e + 1);
    Ok(Fragment { name: name.to_string(), pr, bump, section, body: body[start..end].join("\n") })
}

/// A version's three numbers.
fn parts(v: &str) -> R<(u64, u64, u64)> {
    let parts: Vec<Option<u64>> = v.split('.').map(number).collect();
    let [Some(x), Some(y), Some(z)] = parts[..] else {
        return Err(format!("version {v} is not X.Y.Z"));
    };
    Ok((x, y, z))
}

/// The version after `cur` for one fragment's `bump`: each part raised
/// resets the parts below it (the module docs have the rule).
pub fn next_version(cur: &str, bump: Bump) -> R<String> {
    let (x, y, z) = parts(cur)?;
    Ok(match bump {
        Bump::Breaking => format!("{}.0.0", x + 1),
        Bump::Feature => format!("{x}.{}.0", y + 1),
        Bump::Patch => format!("{x}.{y}.{}", z + 1),
    })
}

/// The version after replaying `bumps`, in merge order, from `cur`; with
/// `release`, (X+1).0.0 whatever they say. No bumps is nothing to release.
pub fn replay(cur: &str, bumps: &[Bump], release: bool) -> R<String> {
    if bumps.is_empty() {
        return Err(NOTHING.into());
    }
    if release {
        let (x, _, _) = parts(cur)?;
        return Ok(format!("{}.0.0", x + 1));
    }
    bumps.iter().try_fold(cur.to_string(), |v, b| next_version(&v, *b))
}

/// The version a release from `cur` with `bumps` pending gets under `to`.
/// `--set` ignores the replay but must be strictly above `cur`.
pub fn target(cur: &str, bumps: &[Bump], to: &To) -> R<String> {
    match to {
        To::Replay => replay(cur, bumps, false),
        To::Release => replay(cur, bumps, true),
        To::Set(v) => {
            if bumps.is_empty() {
                return Err(NOTHING.into());
            }
            if parts(v)? <= parts(cur)? {
                return Err(format!("--set {v} is not above the current version {cur}"));
            }
            Ok(v.clone())
        }
    }
}

/// Sorts `frags` into merge order: by the commit on the first-parent history
/// of HEAD that added each one (a merge commit for a merged pull request,
/// the commit itself for a squash or a rebase), oldest first; a fragment no
/// commit added yet (a dry run's uncommitted one) comes last. Fragments one
/// commit added go by pull request number, then name.
pub fn in_merge_order(root: &Path, frags: &mut [Fragment]) -> R {
    let log = output(Command::new("git").arg("-C").arg(root).args([
        "log",
        "--first-parent",
        "--diff-merges=first-parent",
        "--reverse",
        "--diff-filter=A",
        "--name-only",
        "--format=commit %H",
        "--",
        DIR,
    ]))?;
    let mut added: HashMap<&str, usize> = HashMap::new();
    let mut at = 0;
    for l in log.lines() {
        if l.starts_with("commit ") {
            at += 1;
        } else if let Some(name) = l.strip_prefix("changes/") {
            // A name added again later is pending from its last add.
            added.insert(name, at);
        }
    }
    frags.sort_by(|a, b| {
        let key = |f: &Fragment| added.get(f.name.as_str()).copied().unwrap_or(usize::MAX);
        (key(a), a.pr, &a.name).cmp(&(key(b), b.pr, &b.name))
    });
    Ok(())
}

/// A fragment's body with no blank line between its list items (outside
/// fenced blocks), as CHANGELOG.md sets them.
fn tight(body: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut fence = Fence::default();
    for l in body.lines() {
        if !fence.opaque(l) && l.starts_with("- ") {
            while out.last().is_some_and(|p| p.trim().is_empty()) {
                out.pop();
            }
        }
        out.push(l);
    }
    out.join("\n")
}

/// The CHANGELOG with the fragments folded into a new `## v<version>`
/// section above the newest one: its parts in [`SECTIONS`] order, each
/// part's lines by pull request number, then file name, with no blank line
/// between list items.
pub fn fold(changelog: &str, version: &str, frags: &[Fragment]) -> R<String> {
    let tag = format!("v{version}");
    if frags.is_empty() {
        return Err(NOTHING.into());
    }
    let heading = format!("## {tag}");
    if changelog.lines().any(|l| l == heading) {
        return Err(format!("CHANGELOG.md already has a {heading} section: fragments fold into a new version only"));
    }
    if !frags.iter().any(|f| f.section == Section::Highlight) {
        return Err(format!(
            "no fragment is `section: highlight`, so {tag} would have no Highlights, and the release notes lead with them: add one (changes/README.md)"
        ));
    }
    let mut sorted: Vec<&Fragment> = frags.iter().collect();
    sorted.sort_by(|a, b| (a.pr, &a.name).cmp(&(b.pr, &b.name)));
    let mut section = String::new();
    for (s, _, title) in SECTIONS {
        let bodies: Vec<String> = sorted.iter().filter(|f| f.section == s).map(|f| tight(&f.body)).collect();
        if !bodies.is_empty() {
            section.push_str(&format!("### {title}\n\n{}\n\n", bodies.join("\n")));
        }
    }
    check_section(section.trim_end(), &tag)?;
    let block = format!("{heading}\n\n{section}");
    let mut at = None;
    let mut off = 0;
    // The newest section: a `## v` line outside every fenced block.
    let mut fence = Fence::default();
    for l in changelog.split_inclusive('\n') {
        if !fence.opaque(bare(l)) && l.starts_with("## v") {
            at = Some(off);
            break;
        }
        off += l.len();
    }
    Ok(match at {
        Some(i) => format!("{}{block}{}", &changelog[..i], &changelog[i..]),
        None => format!("{}\n\n{}\n", changelog.trim_end(), block.trim_end()),
    })
}

/// The text with line `i` of `lines` replaced by `with` (its line ending kept).
fn replace_line(lines: &[&str], i: usize, with: &str) -> String {
    let mut out = String::with_capacity(lines.iter().map(|l| l.len()).sum::<usize>() + 8);
    for (j, l) in lines.iter().enumerate() {
        if j == i {
            out.push_str(with);
            out.push_str(eol(l));
        } else {
            out.push_str(l);
        }
    }
    out
}

/// The text with the first line `pick` accepts replaced by `with(line)`
/// (line ending kept); `None` when no line is picked.
fn replace_first(text: &str, pick: impl Fn(&str) -> bool, with: impl Fn(&str) -> String) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let i = lines.iter().position(|l| pick(bare(l)))?;
    Some(replace_line(&lines, i, &with(bare(lines[i]))))
}

/// Cargo.toml with its package version set (the line [`cargo_version`] reads).
pub fn set_cargo_version(text: &str, version: &str) -> R<String> {
    let out = replace_first(text, |l| l.starts_with("version = \""), |_| format!("version = \"{version}\"")).ok_or("no version in Cargo.toml")?;
    debug_assert_eq!(cargo_version(&out).as_deref(), Some(version));
    Ok(out)
}

/// Cargo.lock with atlas's own entry set to `version`: the `[[package]]`
/// entry named `atlas` with no `source` (a registry crate of the same name
/// has one); `None` when it has none.
pub fn set_lock_version(text: &str, version: &str) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        if bare(lines[i]) != "[[package]]" {
            i += 1;
            continue;
        }
        let end = (i + 1..lines.len()).find(|&j| bare(lines[j]).starts_with('[')).unwrap_or(lines.len());
        let entry = &lines[i + 1..end];
        if entry.iter().any(|l| bare(l) == "name = \"atlas\"") && !entry.iter().any(|l| bare(l).starts_with("source = ")) {
            let v = (i + 1..end).find(|&j| bare(lines[j]).starts_with("version = \""))?;
            return Some(replace_line(&lines, v, &format!("version = \"{version}\"")));
        }
        i = end;
    }
    None
}

/// plugin.json with its version set (the line [`plugin_version`] reads),
/// its indent and comma kept.
pub fn set_plugin_version(text: &str, version: &str) -> R<String> {
    let out = replace_first(
        text,
        |l| l.trim_start_matches(' ').starts_with("\"version\":"),
        |l| {
            let indent = &l[..l.len() - l.trim_start_matches(' ').len()];
            let comma = if l.trim_end().ends_with(',') { "," } else { "" };
            format!("{indent}\"version\": \"{version}\"{comma}")
        },
    )
    .ok_or("no version in plugin.json")?;
    debug_assert_eq!(plugin_version(&out).as_deref(), Some(version));
    Ok(out)
}

/// The pending changes in a `changes/` folder.
#[derive(Debug, Default)]
pub struct Pending {
    /// The fragments, by pull request then name.
    pub fragments: Vec<Fragment>,
    /// Every fragment and marker file name, which a release deletes.
    pub files: Vec<String>,
}

/// A folder's entries: each name, and whether it is a folder. A missing
/// folder has none.
fn names_in(dir: &Path) -> R<Vec<(String, bool)>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut names = Vec::new();
    for e in rd {
        let e = e.map_err(|e| format!("{}: {e}", dir.display()))?;
        names.push((e.file_name().to_string_lossy().into_owned(), e.file_type().map_err(|e| e.to_string())?.is_dir()));
    }
    names.sort();
    Ok(names)
}

/// Reads a `changes/` folder (a missing one holds nothing), refusing with
/// every malformed name and fragment at once.
pub fn pending(dir: &Path) -> R<Pending> {
    let mut p = Pending::default();
    let mut errors = Vec::new();
    for (n, is_dir) in names_in(dir)? {
        if is_dir {
            errors.push(format!("changes/{n}: a folder; fragments are files"));
            continue;
        }
        match entry(&n) {
            Err(e) => errors.push(e),
            Ok(Entry::Readme) => {}
            Ok(Entry::None(_)) => p.files.push(n),
            Ok(Entry::Fragment(_)) => {
                let path = dir.join(&n);
                match std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display())).and_then(|b| parse_fragment_bytes(&n, &b)) {
                    Ok(f) => {
                        p.fragments.push(f);
                        p.files.push(n);
                    }
                    Err(e) => errors.push(e),
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(format!("changes/ holds what is not a fragment (changes/README.md has the format):\n{}", errors.join("\n")));
    }
    p.fragments.sort_by(|a, b| (a.pr, &a.name).cmp(&(b.pr, &b.name)));
    Ok(p)
}

/// Refuses a release while fragments are pending: their version step has not
/// been merged. Only fragment-form names count (a malformed fragment too, a
/// stray file not); it reads no tag.
pub fn refuse_pending(root: &Path) -> R {
    let names: Vec<String> = names_in(&root.join(DIR))?
        .into_iter()
        .filter(|(n, is_dir)| !is_dir && matches!(entry(n), Ok(Entry::Fragment(_))))
        .map(|(n, _)| n)
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} changelog fragment(s) in changes/ are not in a release yet ({}): run `cargo xtask version`, merge its commit, then release that version",
        names.len(),
        names.join(", ")
    ))
}

/// The command, in the repository at `root`; the new version.
pub fn run_in(root: &Path, dry_run: bool, to: &To) -> R<String> {
    let git = |args: &[&str]| output(Command::new("git").arg("-C").arg(root).args(args));
    if !dry_run {
        if !git(&["status", "--porcelain", "--untracked-files=no"])?.trim().is_empty() {
            return Err("uncommitted changes: the version commit holds only its own".into());
        }
        let loose = git(&["status", "--porcelain", "--untracked-files=all", "--", DIR])?;
        if !loose.trim().is_empty() {
            return Err(format!("files in changes/ are not committed, so the release would miss them: {}", loose.trim()));
        }
    }
    let mut p = pending(&root.join(DIR))?;
    if p.fragments.is_empty() {
        return Err(NOTHING.into());
    }
    if !dry_run {
        // What the release deletes must be what git holds: an ignored
        // fragment would be folded and then left behind.
        let tracked = git(&["ls-files", "--", DIR])?;
        let tracked: Vec<&str> = tracked.lines().filter_map(|l| l.strip_prefix("changes/")).collect();
        let stray: Vec<String> = p.files.iter().filter(|f| !tracked.contains(&f.as_str())).map(|f| format!("{DIR}/{f}")).collect();
        if !stray.is_empty() {
            return Err(format!(
                "{} not tracked by git (ignored?): commit or remove it, or the release would fold it and leave it behind",
                stray.join(", ")
            ));
        }
    }
    let cur = crate::release_ci::version_of(root)?;
    in_merge_order(root, &mut p.fragments)?;
    let mut steps = Vec::new();
    let mut v = cur.clone();
    for f in &p.fragments {
        v = next_version(&v, f.bump)?;
        steps.push(format!("{} {} {v}", f.name, f.bump.name()));
    }
    let bumps: Vec<Bump> = p.fragments.iter().map(|f| f.bump).collect();
    let next = target(&cur, &bumps, to)?;
    let how = match to {
        To::Replay => format!("{cur} -> {next}"),
        To::Release => format!("{cur} -> {next} by --release (the fragments alone: {v})"),
        To::Set(_) => format!("{cur} -> {next} by --set (the fragments alone: {v})"),
    };
    let tag = format!("v{next}");
    say(&format!("{} fragment(s) in merge order: {}", p.fragments.len(), steps.join(", ")));
    say(&how);
    let changelog = fold(&read_text(&root.join("CHANGELOG.md"))?, &next, &p.fragments)?;
    if dry_run {
        println!("## {tag}\n\n{}", crate::release::changelog_section(&changelog, &tag));
        say("dry run: nothing written");
        return Ok(next);
    }
    let consumed: Vec<String> = p.files.iter().map(|f| format!("{DIR}/{f}")).collect();
    let lock = root.join("Cargo.lock");
    let lock_text = if lock.is_file() { Some(read_text(&lock)?) } else { None };
    let new_lock = lock_text.as_deref().and_then(|l| set_lock_version(l, &next));
    let mut touched: Vec<String> = ["Cargo.toml", ".claude-plugin/plugin.json", "CHANGELOG.md"].map(String::from).to_vec();
    if new_lock.is_some() {
        touched.push("Cargo.lock".into());
    }
    // Every file as it was, to put back if any step below fails.
    let mut before: Vec<(String, Vec<u8>)> = Vec::new();
    for f in touched.iter().chain(&consumed) {
        let path = root.join(f);
        before.push((f.clone(), std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?));
    }
    let commit = || -> R {
        write(&root.join("Cargo.toml"), set_cargo_version(&read_text(&root.join("Cargo.toml"))?, &next)?)?;
        if let Some(l) = &new_lock {
            write(&lock, l)?;
        }
        let plugin = root.join(".claude-plugin/plugin.json");
        write(&plugin, set_plugin_version(&read_text(&plugin)?, &next)?)?;
        write(&root.join("CHANGELOG.md"), &changelog)?;
        let written = crate::release_ci::version_of(root)?;
        if written != next {
            return Err(format!("wrote {next} but Cargo.toml and plugin.json now read {written}"));
        }
        let mut add = vec!["add", "--"];
        add.extend(touched.iter().map(String::as_str));
        git(&add)?;
        let mut rm = vec!["rm", "-q", "--"];
        rm.extend(consumed.iter().map(String::as_str));
        git(&rm)?;
        let msg = format!(
            "Release {tag}\n\nFolds {} changelog fragment(s) into CHANGELOG.md, replayed in merge order\n({how}): {}.\nSets the version in Cargo.toml, Cargo.lock and plugin.json, and deletes the\nfragments and markers it read: {}.",
            p.fragments.len(),
            steps.join(", "),
            consumed.join(", ")
        );
        git(&["commit", "-q", "-m", &msg])?;
        Ok(())
    };
    if let Err(e) = commit() {
        let mut paths = vec!["reset", "-q", "HEAD", "--"];
        paths.extend(before.iter().map(|(f, _)| f.as_str()));
        let unstaged = git(&paths).err();
        let mut put_back: Vec<String> = Vec::new();
        for (f, bytes) in &before {
            if let Err(w) = write(&root.join(f), bytes) {
                put_back.push(w);
            }
        }
        return Err(match (unstaged, put_back.is_empty()) {
            (None, true) => format!("{e}\nrolled back: every file is as it was and nothing was committed"),
            (u, _) => format!("{e}\nthe rollback failed too, so check `git status`: {}", u.into_iter().chain(put_back).collect::<Vec<_>>().join("; ")),
        });
    }
    say(&format!("committed {tag}; merge it through a pull request, then `cargo xtask release ship {tag}`"));
    Ok(next)
}

/// The command line: `Some((dry_run, to))`, or `None` for help.
pub fn parse_args(args: Vec<String>) -> R<Option<(bool, To)>> {
    let (mut dry_run, mut release, mut set) = (false, false, None::<String>);
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        let value = match a.as_str() {
            "--dry-run" => {
                dry_run = true;
                continue;
            }
            "--release" => {
                release = true;
                continue;
            }
            "-h" | "--help" => return Ok(None),
            "--set" => args.next().ok_or_else(|| format!("--set needs a version, X.Y.Z\n{USAGE}"))?,
            _ => match a.strip_prefix("--set=") {
                Some(v) => v.to_string(),
                None => return Err(format!("unknown argument {a}\n{USAGE}")),
            },
        };
        parts(&value)?;
        if set.replace(value).is_some() {
            return Err(format!("--set twice\n{USAGE}"));
        }
    }
    let to = match (release, set) {
        (true, Some(_)) => return Err(format!("--set and --release together: --set names the version outright\n{USAGE}")),
        (true, None) => To::Release,
        (false, Some(v)) => To::Set(v),
        (false, None) => To::Replay,
    };
    Ok(Some((dry_run, to)))
}

/// `cargo xtask version`.
pub fn run(args: Vec<String>) -> R {
    let Some((dry_run, to)) = parse_args(args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    run_in(&crate::release::repo_root(), dry_run, &to).map(|_| ())
}

/// A POSIX sh to run `scripts/changes-check.sh` with: `sh` on PATH, or on
/// Windows Git for Windows' own (never WSL's `bash`). The tests' only.
#[cfg(test)]
pub fn posix_sh() -> Option<std::path::PathBuf> {
    if !cfg!(windows) {
        return Some(std::path::PathBuf::from("sh"));
    }
    let exec = output(Command::new("git").arg("--exec-path")).ok()?;
    let exec = std::path::PathBuf::from(exec.trim());
    exec.ancestors().find_map(|a| ["usr/bin/sh.exe", "bin/sh.exe"].iter().map(|c| a.join(c)).find(|p| p.is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::repo_root;
    use crate::release_ci::tests::{sh, tmp};
    use std::path::PathBuf;
    use Bump::{Breaking, Feature, Patch};

    fn frag(bump: &str, section: &str, body: &str) -> String {
        format!("---\nbump: {bump}\nsection: {section}\n---\n{body}\n")
    }

    fn parsed(name: &str, bump: &str, section: &str, body: &str) -> Fragment {
        parse_fragment(name, &frag(bump, section, body)).unwrap()
    }

    #[test]
    fn fragment_names() {
        assert_eq!(entry("187.md"), Ok(Entry::Fragment(187)));
        assert_eq!(entry("187-docs-page.md"), Ok(Entry::Fragment(187)));
        assert_eq!(entry("42.none"), Ok(Entry::None(42)));
        assert_eq!(entry("README.md"), Ok(Entry::Readme));
        for bad in ["readme.md", "0187.md", "187", "187.txt", "187-.md", "187-Docs.md", "x187.md", "187.none.md", "-1.md", "187 .md", "187_a.md"] {
            assert!(entry(bad).unwrap_err().contains("not a fragment name"), "{bad}");
        }
    }

    #[test]
    fn parses_a_good_fragment() {
        let f = parse_fragment("12.md", "---\nbump: feature\nsection: feature\n---\n\n- **A thing.** It does\n  more.\n- Another.\n\n").unwrap();
        assert_eq!(f, Fragment { name: "12.md".into(), pr: 12, bump: Feature, section: Section::Feature, body: "- **A thing.** It does\n  more.\n- Another.".into() });
        // CRLF, blank front matter lines, spaces around the colon, and a fenced block with a `#` line.
        let f = parse_fragment("3-x.md", "---\r\n\r\nsection :  known-limit\r\nbump:breaking\r\n---\r\n- a\r\n  ```\r\n  # not a heading\r\n  ```\r\n").unwrap();
        assert_eq!((f.pr, f.bump, f.section), (3, Breaking, Section::KnownLimit));
        assert_eq!(f.body, "- a\n  ```\n  # not a heading\n  ```");
        for (s, key, _) in SECTIONS {
            let bump = if s == Section::Breaking { "breaking" } else { "patch" };
            assert_eq!(parsed("1.md", bump, key, "- x").section, s);
        }
        assert_eq!(parse_fragment_bytes("1.md", frag("patch", "fix", "- caf\u{e9}").as_bytes()).unwrap().body, "- caf\u{e9}");
    }

    /// Malformed fragments, each with what the refusal says. The same set
    /// runs through scripts/changes-check.sh below.
    pub(crate) const BAD: &[(&str, &str)] = &[
        ("", "front matter"),
        ("bump: feature\nsection: fix\n---\n- a\n", "front matter"),
        ("---\nbump: feature\nsection: fix\n", "closing ---"),
        ("---\nbump: feature\nsection: fix\n- a\n", "key: value"),
        ("---\nsection: fix\n---\n- a\n", "no bump"),
        ("---\nbump: feature\n---\n- a\n", "no section"),
        ("---\nbump: huge\nsection: fix\n---\n- a\n", "bump must be"),
        ("---\nbump: Feature\nsection: fix\n---\n- a\n", "bump must be"),
        // The cargo words are not this vocabulary.
        ("---\nbump: minor\nsection: fix\n---\n- a\n", "bump must be"),
        ("---\nbump: major\nsection: fix\n---\n- a\n", "bump must be"),
        ("---\nbump: feature\nsection: fixes\n---\n- a\n", "section must be"),
        ("---\nbump: feature\nbump: patch\nsection: fix\n---\n- a\n", "duplicate"),
        ("---\nbump: feature\nsection: fix\npr: 3\n---\n- a\n", "unknown"),
        ("---\nbump feature\nsection: fix\n---\n- a\n", "key: value"),
        ("---\nbump: feature\nsection: fix\n---\n", "body is empty"),
        ("---\nbump: feature\nsection: fix\n---\n\n   \n", "body is empty"),
        ("---\nbump: feature\nsection: fix\n---\nA paragraph.\n", "a list"),
        ("---\nbump: feature\nsection: fix\n---\n<!-- later -->\n- a\n", "a list"),
        ("---\nbump: feature\nsection: fix\n---\n- a\n### Fixes\n- b\n", "heading"),
        ("---\nbump: feature\nsection: fix\n---\n- a\n## v9.9.9\n", "heading"),
        ("---\nbump: feature\nsection: breaking\n---\n- a\n", "bump: breaking"),
        ("---\nbump: patch\nsection: breaking\n---\n- a\n", "bump: breaking"),
        ("\u{feff}---\nbump: patch\nsection: fix\n---\n- a\n", "byte order mark"),
        // A fence is up to three spaces, then three or more backticks or
        // tildes (CommonMark), and must close: an open one would swallow
        // the sections after the fragment.
        ("---\nbump: patch\nsection: fix\n---\n- a\n  ```\n  ## v0.0.1\n", "unclosed fenced block"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  ~~~\n  # x\n", "unclosed fenced block"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n```\n~~~\n", "unclosed fenced block"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n````\n```\n", "unclosed fenced block"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n```\n``` x\n", "unclosed fenced block"),
        // Four spaces or a tab before the backticks is no fence, so the
        // `#` line after it is a heading.
        ("---\nbump: patch\nsection: fix\n---\n- a\n    ```\n# x\n    ```\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n\t```\n# x\n\t```\n", "heading"),
        // Nor is a code span at the start of a line.
        ("---\nbump: patch\nsection: fix\n---\n- a\n```b```\n# x\n```b```\n", "heading"),
        // A line outside a fence whose first non-space character is `<`
        // starts an HTML block on GitHub, which can hide everything after
        // it (an open comment, `<details>`): refused, comments included.
        ("---\nbump: patch\nsection: fix\n---\n- a\n<!--\n- b\n", "starts with <"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n<!-- a whole comment -->\n", "starts with <"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n<details>\n\n- b\n</details>\n", "starts with <"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  <br>\n", "starts with <"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n\t<b>x</b>\n", "starts with <"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n- b\n   <!-- x -->\n", "starts with <"),
        // Once a fence opens at indent k, a line indented less than k ends
        // the list item, and the fence with it, on GitHub: refused until
        // the fence closes, its closing line included.
        ("---\nbump: patch\nsection: fix\n---\n- a\n  ```\n- b\n## v0.0.1\n```\n", "indented less than"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  ```\n## v0.0.1\n", "indented less than"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  ~~~\n# x\n  ~~~\n", "indented less than"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n   ```\n  x\n   ```\n", "indented less than"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  ```\n  x\n```\n", "indented less than"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  ```\n\n x\n  ```\n", "indented less than"),
        // A C0 control character other than a tab or a line ending: GitHub
        // reads a lone CR as a line break, which would start a comment, a
        // heading or a fence the line rules never saw.
        ("---\nbump: patch\nsection: fix\n---\n- a\r<!--\n- b\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a\r## v0.0.1\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a\r```\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a\r\r\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a\r", "control character"),
        ("---\r\nbump: patch\rsection: fix\r\n---\r\n- a\r\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a\u{0}b\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a\u{b}b\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a\u{c}b\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a \u{1b}[2J\n", "control character"),
        ("---\nbump: patch\nsection: fix\n---\n- a\u{1f}\n", "control character"),
        // A heading after an indent or a list marker renders as a heading
        // too, as does a setext underline under a line of text.
        ("---\nbump: patch\nsection: fix\n---\n- ## v9.9.9\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  ## v9.9.9\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n\t# x\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n1. # x\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  2) ###### x\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n* #\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n+ - #\tx\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  > # x\n", "heading"),
        ("---\nbump: patch\nsection: fix\n---\n- v9.9.9\n  ===\n", "underline"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n  b\n  ---\n", "underline"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n---\n", "underline"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n=\n", "underline"),
        ("---\nbump: patch\nsection: fix\n---\n- a\n\t-- \n", "underline"),
    ];

    /// Malformed fragments that are not text at all.
    pub(crate) const BAD_BYTES: &[(&[u8], &str)] = &[
        (b"---\nbump: patch\nsection: fix\n---\n- caf\xe9 (Latin-1)\n", "UTF-8"),
        (b"\xff\xfe-\x00-\x00-\x00\n\x00", "UTF-8"),
    ];

    #[test]
    fn refuses_malformed_fragments() {
        for (text, want) in BAD {
            let e = parse_fragment("5.md", text).expect_err(text);
            assert!(e.starts_with("changes/5.md: "), "{e}");
            assert!(e.contains(want), "{text:?}: {e}");
        }
        for (bytes, want) in BAD_BYTES {
            let e = parse_fragment_bytes("5.md", bytes).expect_err("not UTF-8");
            assert!(e.starts_with("changes/5.md: ") && e.contains(want), "{bytes:?}: {e}");
        }
    }

    #[test]
    fn each_bump_resets_the_parts_below_it() {
        assert_eq!(Bump::parse("breaking"), Some(Breaking));
        assert_eq!(Bump::parse("feature"), Some(Feature));
        assert_eq!(Bump::parse("patch"), Some(Patch));
        assert_eq!(Breaking.name(), "breaking");
        // The same rule above and below 1.0.0, and at 0.0.z.
        for (cur, bump, next) in [
            ("0.5.0", Patch, "0.5.1"),
            ("0.5.3", Patch, "0.5.4"),
            ("0.5.0", Feature, "0.6.0"),
            ("0.5.3", Feature, "0.6.0"),
            ("0.5.3", Breaking, "1.0.0"),
            ("0.0.7", Patch, "0.0.8"),
            ("0.0.7", Feature, "0.1.0"),
            ("0.0.7", Breaking, "1.0.0"),
            ("1.2.3", Patch, "1.2.4"),
            ("1.2.3", Feature, "1.3.0"),
            ("1.2.3", Breaking, "2.0.0"),
            ("9.19.99", Feature, "9.20.0"),
        ] {
            assert_eq!(next_version(cur, bump).as_deref(), Ok(next), "{cur} {bump:?}");
        }
        for bad in ["0.5", "0.5.0.1", "0.5.0-rc1", "v0.5.0", "0.+5.0", "0.5.x", ""] {
            assert!(next_version(bad, Patch).unwrap_err().contains("not X.Y.Z"), "{bad}");
        }
    }

    #[test]
    fn replay_takes_each_fragment_in_turn() {
        // Each step counts, in order, and resets what is below it.
        assert_eq!(replay("0.6.0", &[Patch, Patch, Feature, Patch], false).as_deref(), Ok("0.7.1"));
        let mut fifteen = vec![Patch; 15];
        fifteen.push(Feature);
        assert_eq!(replay("0.5.0", &fifteen, false).as_deref(), Ok("0.6.0"), "the patches before the feature leave no trace");
        assert_eq!(replay("0.5.0", &[Feature, Patch, Patch], false).as_deref(), Ok("0.6.2"));
        assert_eq!(replay("0.5.0", &[Patch, Patch, Feature], false).as_deref(), Ok("0.6.0"));
        assert_eq!(replay("0.5.0", &[Feature, Feature], false).as_deref(), Ok("0.7.0"));
        // A breaking fragment forces a release bump.
        assert_eq!(replay("0.5.3", &[Breaking], false).as_deref(), Ok("1.0.0"));
        assert_eq!(replay("1.2.3", &[Feature, Breaking, Patch], false).as_deref(), Ok("2.0.1"));
        assert_eq!(replay("0.5.0", &[Breaking, Breaking], false).as_deref(), Ok("2.0.0"));
        assert!(replay("0.5.0", &[], false).unwrap_err().contains("no changelog fragments"));
    }

    #[test]
    fn the_release_flag_raises_the_release_number_by_hand() {
        // `--release` makes it (X+1).0.0, whatever the fragments say.
        assert_eq!(replay("0.5.0", &[Patch], true).as_deref(), Ok("1.0.0"));
        assert_eq!(replay("0.5.3", &[Feature, Patch], true).as_deref(), Ok("1.0.0"));
        assert_eq!(replay("1.2.3", &[Patch], true).as_deref(), Ok("2.0.0"));
        assert_eq!(replay("0.5.0", &[Patch, Breaking, Patch], true).as_deref(), Ok("1.0.0"));
        assert_eq!(replay("0.5.0", &[Breaking, Breaking], true).as_deref(), Ok("1.0.0"));
        assert!(replay("0.5.0", &[], true).unwrap_err().contains("no changelog fragments"));
        // The command line.
        let a = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()).collect());
        assert_eq!(a(&[]), Ok(Some((false, To::Replay))));
        assert_eq!(a(&["--release", "--dry-run"]), Ok(Some((true, To::Release))));
        assert_eq!(a(&["--release"]), Ok(Some((false, To::Release))));
        assert_eq!(a(&["--help"]), Ok(None));
        assert!(a(&["--major"]).unwrap_err().contains("unknown argument --major"));
    }

    const HEAD: &str = "# Changelog\n\nThe header.\n\n- `### Highlights`: not a section.\n\n";
    const OLD: &str = "## v0.5.0\n\n### Highlights\n\n- old\n";

    #[test]
    fn fold_keeps_the_section_order_and_the_older_sections() {
        let frags = vec![
            parsed("9.md", "patch", "fix", "- fix nine"),
            parsed("2.md", "feature", "known-limit", "- limit two"),
            parsed("7.md", "feature", "feature", "- feature seven\n  wrapped"),
            parsed("3-b.md", "patch", "fix", "- fix three b"),
            parsed("3-a.md", "patch", "fix", "- fix three a"),
            parsed("4.md", "breaking", "breaking", "- breaks four"),
            parsed("5.md", "feature", "highlight", "- shines five\n\n- and five more\n```\n\n- not an item\n```"),
            parsed("6.md", "patch", "upgrade", "- upgrade six"),
            parsed("8.md", "patch", "security", "- harder eight"),
            parsed("1.md", "feature", "highlight", "- shines one"),
        ];
        let got = fold(&format!("{HEAD}{OLD}"), "0.6.0", &frags).unwrap();
        // No blank line between bullets, as in CHANGELOG.md (a fenced block keeps its own).
        let want = "## v0.6.0\n\n\
            ### Highlights\n\n- shines one\n- shines five\n- and five more\n```\n\n- not an item\n```\n\n\
            ### Breaking changes\n\n- breaks four\n\n\
            ### Upgrade notes\n\n- upgrade six\n\n\
            ### Features\n\n- feature seven\n  wrapped\n\n\
            ### Fixes\n\n- fix three a\n- fix three b\n- fix nine\n\n\
            ### Security hardening\n\n- harder eight\n\n\
            ### Known limits\n\n- limit two\n\n";
        assert_eq!(got, format!("{HEAD}{want}{OLD}"));
        // Empty parts are left out.
        let got = fold(&format!("{HEAD}{OLD}"), "0.5.1", &frags[8..]).unwrap();
        assert_eq!(got, format!("{HEAD}## v0.5.1\n\n### Highlights\n\n- shines one\n\n### Security hardening\n\n- harder eight\n\n{OLD}"));
        // A CHANGELOG with no section yet gets the first at its end.
        let got = fold("# Changelog\n\nThe header.\n", "0.1.0", &frags[9..]).unwrap();
        assert_eq!(got, "# Changelog\n\nThe header.\n\n## v0.1.0\n\n### Highlights\n\n- shines one\n");
    }

    /// The fold finds the newest section past fenced `## v` lines (indented,
    /// or tildes), and keeps a tilde fence's blank lines as it keeps a
    /// backtick fence's.
    #[test]
    fn fold_reads_past_fenced_version_lines() {
        let head = "# Changelog\n\nA section looks like this:\n\n  ~~~\n## v9.9.9\n  ~~~\n\n   ```md\n## v8.8.8\n   ```\n\n";
        // A fence at column 0, so the `- ` line inside it is no short line.
        let frags = [parsed("1.md", "feature", "highlight", "- one\n~~~\n\n- not an item\n~~~")];
        let got = fold(&format!("{head}{OLD}"), "0.6.0", &frags).unwrap();
        assert_eq!(got, format!("{head}## v0.6.0\n\n### Highlights\n\n- one\n~~~\n\n- not an item\n~~~\n\n{OLD}"));
    }

    #[test]
    fn fold_refuses_no_highlights_and_a_section_that_exists() {
        let e = fold(&format!("{HEAD}{OLD}"), "0.5.1", &[parsed("9.md", "patch", "fix", "- fix")]).unwrap_err();
        assert!(e.contains("no Highlights") && e.contains("section: highlight"), "{e}");
        let e = fold(&format!("{HEAD}{OLD}"), "0.5.0", &[parsed("1.md", "patch", "highlight", "- h")]).unwrap_err();
        assert!(e.contains("already has a ## v0.5.0 section"), "{e}");
        assert!(fold(OLD, "0.5.1", &[]).unwrap_err().contains("no changelog fragments"));
    }

    #[test]
    fn writes_the_version_into_each_file() {
        let toml = "[package]\nname = \"atlas\"\nversion = \"0.5.0\"\nedition = \"2021\"\n\n[dependencies]\nx = { version = \"1\" }\n";
        let t = set_cargo_version(toml, "0.6.0").unwrap();
        assert_eq!(t, toml.replace("version = \"0.5.0\"", "version = \"0.6.0\""));
        assert_eq!(cargo_version(&t).as_deref(), Some("0.6.0"));
        assert!(set_cargo_version("[package]\nname = \"x\"\n", "1.0.0").unwrap_err().contains("no version"));

        let lock = "version = 4\n\n[[package]]\nname = \"aho\"\nversion = \"0.5.0\"\n\n[[package]]\nname = \"atlas\"\nversion = \"0.5.0\"\ndependencies = [\n \"aho\",\n]\n\n[[package]]\nname = \"atlas-x\"\nversion = \"0.5.0\"\n";
        let l = set_lock_version(lock, "0.6.0").unwrap();
        assert_eq!(l, lock.replacen("name = \"atlas\"\nversion = \"0.5.0\"", "name = \"atlas\"\nversion = \"0.6.0\"", 1));
        assert_eq!(set_lock_version("version = 4\n[[package]]\nname = \"other\"\nversion = \"1.0.0\"\n", "0.6.0"), None);
        // Anchored to atlas's own [[package]] entry: not a registry crate of
        // the same name (it has a source), nor a name outside an entry.
        let reg = "version = 4\n\n[[package]]\nname = \"atlas\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"ab\"\n\n[[package]]\nname = \"atlas\"\nversion = \"0.5.0\"\ndependencies = [\n \"atlas 0.1.0\",\n]\n";
        assert_eq!(set_lock_version(reg, "0.6.0").unwrap(), reg.replacen("version = \"0.5.0\"", "version = \"0.6.0\"", 1));
        assert_eq!(set_lock_version("version = 4\n\n[metadata]\nname = \"atlas\"\nversion = \"0.5.0\"\n", "0.6.0"), None);
        let crlf = "version = 4\r\n\r\n[[package]]\r\nname = \"atlas\"\r\nversion = \"0.5.0\"\r\n";
        assert_eq!(set_lock_version(crlf, "0.6.0").unwrap(), crlf.replace("0.5.0", "0.6.0"));

        let plugin = "{\n  \"name\": \"atlas\",\n  \"version\": \"0.5.0\",\n  \"author\": {\n    \"version\": \"9\"\n  }\n}\n";
        let p = set_plugin_version(plugin, "0.6.0").unwrap();
        assert_eq!(p, plugin.replacen("0.5.0", "0.6.0", 1));
        assert_eq!(plugin_version(&p).as_deref(), Some("0.6.0"));
        let last = set_plugin_version("{\n  \"version\": \"0.5.0\"\n}\n", "0.6.0").unwrap();
        assert_eq!(last, "{\n  \"version\": \"0.6.0\"\n}\n");
        assert!(set_plugin_version("{}\n", "0.6.0").unwrap_err().contains("no version"));
    }

    /// A git repository shaped like atlas's top level, at 0.5.0.
    fn repo(name: &str, changes: &[(&str, &str)]) -> PathBuf {
        let d = tmp(&format!("version-{name}"));
        sh(&d, &["init", "-q", "-b", "main"]);
        sh(&d, &["config", "user.name", "D"]);
        sh(&d, &["config", "user.email", "dev@example.invalid"]);
        let files: Vec<(String, String)> = [
            ("Cargo.toml", "[package]\nname = \"atlas\"\nversion = \"0.5.0\"\n"),
            ("Cargo.lock", "version = 4\n\n[[package]]\nname = \"atlas\"\nversion = \"0.5.0\"\n"),
            (".claude-plugin/plugin.json", "{\n  \"name\": \"atlas\",\n  \"version\": \"0.5.0\",\n  \"license\": \"x\"\n}\n"),
            ("CHANGELOG.md", "# Changelog\n\nHeader.\n\n## v0.5.0\n\n### Highlights\n\n- old\n"),
            ("changes/README.md", "# Fragments\n"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .chain(changes.iter().map(|(a, b)| (format!("changes/{a}"), b.to_string())))
        .collect();
        for (f, t) in &files {
            std::fs::create_dir_all(d.join(f).parent().unwrap()).unwrap();
            std::fs::write(d.join(f), t).unwrap();
        }
        sh(&d, &["add", "-A"]);
        sh(&d, &["commit", "-q", "-m", "seed"]);
        d
    }

    /// Adds `name` to changes/ in its own commit.
    fn add(d: &Path, name: &str, text: &str) {
        std::fs::write(d.join("changes").join(name), text).unwrap();
        sh(d, &["add", "--", &format!("changes/{name}")]);
        sh(d, &["commit", "-q", "-m", &format!("add {name}")]);
    }

    #[test]
    fn version_folds_bumps_and_commits_in_one() {
        let d = repo(
            "fold",
            &[
                ("11.md", &frag("feature", "feature", "- feature eleven")),
                ("12.md", &frag("patch", "highlight", "- shines twelve")),
                ("13.none", ""),
            ],
        );
        let head = sh(&d, &["rev-parse", "HEAD"]);
        // A dry run changes nothing. One commit added both: by number, 11 then 12.
        assert_eq!(run_in(&d, true, &To::Replay).unwrap(), "0.6.1");
        assert_eq!(sh(&d, &["rev-parse", "HEAD"]), head);
        assert_eq!(sh(&d, &["status", "--porcelain"]), "");

        assert_eq!(run_in(&d, false, &To::Replay).unwrap(), "0.6.1");
        assert_eq!(sh(&d, &["rev-parse", "HEAD~1"]), head);
        assert_eq!(sh(&d, &["status", "--porcelain"]), "", "everything is in the one commit");
        let changed = sh(&d, &["diff", "--name-status", "HEAD~1", "HEAD"]);
        assert_eq!(
            changed.lines().collect::<Vec<_>>(),
            [
                "M\t.claude-plugin/plugin.json",
                "M\tCHANGELOG.md",
                "M\tCargo.lock",
                "M\tCargo.toml",
                "D\tchanges/11.md",
                "D\tchanges/12.md",
                "D\tchanges/13.none",
            ]
        );
        assert_eq!(crate::release_ci::version_of(&d).unwrap(), "0.6.1");
        assert!(std::fs::read_to_string(d.join("Cargo.lock")).unwrap().contains("name = \"atlas\"\nversion = \"0.6.1\""));
        let cl = std::fs::read_to_string(d.join("CHANGELOG.md")).unwrap();
        assert_eq!(cl, "# Changelog\n\nHeader.\n\n## v0.6.1\n\n### Highlights\n\n- shines twelve\n\n### Features\n\n- feature eleven\n\n## v0.5.0\n\n### Highlights\n\n- old\n");
        assert!(d.join("changes/README.md").is_file());
        let msg = sh(&d, &["log", "-1", "--format=%B"]);
        assert!(msg.starts_with("Release v0.6.1") && msg.contains("0.5.0 -> 0.6.1"), "{msg}");
        // Nothing is pending now.
        assert!(run_in(&d, false, &To::Replay).unwrap_err().contains("no changelog fragments"));
    }

    #[test]
    fn version_replays_the_fragments_in_merge_order() {
        let d = repo("order", &[("9.md", &frag("patch", "highlight", "- shines nine"))]);
        // A side branch adds 10.md, a feature, before main adds 30.md, a
        // patch; the side branch merges after that.
        sh(&d, &["checkout", "-q", "-b", "side"]);
        add(&d, "10.md", &frag("feature", "feature", "- feature ten"));
        sh(&d, &["checkout", "-q", "main"]);
        add(&d, "30.md", &frag("patch", "fix", "- fix thirty"));
        sh(&d, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        add(&d, "40.md", &frag("patch", "fix", "- fix forty"));
        // Merge order 9, 30, 10, 40: 0.5.1, 0.5.2, 0.6.0, 0.6.1. By number
        // (9, 10, 30, 40) it would be 0.6.2, and by the commit that wrote
        // each (10 before 30) too.
        let names = |frags: Vec<Fragment>| frags.into_iter().map(|f| f.name).collect::<Vec<_>>();
        let mut p = pending(&d.join(DIR)).unwrap();
        in_merge_order(&d, &mut p.fragments).unwrap();
        assert_eq!(names(p.fragments), ["9.md", "30.md", "10.md", "40.md"]);
        assert_eq!(run_in(&d, true, &To::Replay).unwrap(), "0.6.1");
        assert_eq!(run_in(&d, false, &To::Replay).unwrap(), "0.6.1");
        // One section for the release, not one per step.
        let cl = std::fs::read_to_string(d.join("CHANGELOG.md")).unwrap();
        assert_eq!(cl.matches("\n## v").count(), 2, "{cl}");
        assert!(cl.contains("## v0.6.1\n\n### Highlights\n\n- shines nine\n\n### Features\n\n- feature ten\n\n### Fixes\n\n- fix thirty\n- fix forty\n\n## v0.5.0"), "{cl}");
        let msg = sh(&d, &["log", "-1", "--format=%B"]);
        assert!(msg.contains("9.md patch 0.5.1, 30.md patch 0.5.2, 10.md feature 0.6.0, 40.md patch 0.6.1"), "{msg}");
    }

    #[test]
    fn version_with_the_release_flag_and_a_breaking_fragment() {
        let d = repo("release", &[("12.md", &frag("feature", "highlight", "- shines twelve"))]);
        assert_eq!(run_in(&d, true, &To::Replay).unwrap(), "0.6.0");
        assert_eq!(run_in(&d, false, &To::Release).unwrap(), "1.0.0");
        assert_eq!(crate::release_ci::version_of(&d).unwrap(), "1.0.0");
        assert!(std::fs::read_to_string(d.join("CHANGELOG.md")).unwrap().contains("## v1.0.0\n\n### Highlights\n\n- shines twelve\n\n## v0.5.0"));
        assert!(sh(&d, &["log", "-1", "--format=%B"]).contains("--release"));

        let d = repo("breaking", &[("12.md", &frag("breaking", "breaking", "- breaks twelve")), ("13.md", &frag("patch", "highlight", "- shines"))]);
        assert_eq!(run_in(&d, false, &To::Replay).unwrap(), "1.0.1");
        assert!(std::fs::read_to_string(d.join("CHANGELOG.md")).unwrap().contains("## v1.0.1\n\n### Highlights\n\n- shines\n\n### Breaking changes\n\n- breaks twelve\n\n"));
    }

    #[test]
    fn the_set_flag_names_the_version_outright() {
        // `--set X.Y.Z` is the version, whatever the fragments say, if it is
        // above the current one.
        let set = |v: &str| To::Set(v.into());
        assert_eq!(target("0.5.0", &[Patch, Patch], &set("0.6.0")).as_deref(), Ok("0.6.0"));
        assert_eq!(target("0.5.0", &[Breaking], &set("0.6.0")).as_deref(), Ok("0.6.0"));
        assert_eq!(target("0.5.3", &[Patch], &set("0.5.4")).as_deref(), Ok("0.5.4"));
        assert_eq!(target("0.5.0", &[Patch], &set("2.0.0")).as_deref(), Ok("2.0.0"));
        assert_eq!(target("0.9.9", &[Patch], &set("0.10.0")).as_deref(), Ok("0.10.0"));
        for low in ["0.5.0", "0.4.9", "0.0.1"] {
            let e = target("0.5.0", &[Patch], &set(low)).unwrap_err();
            assert!(e.contains(&format!("--set {low} is not above the current version 0.5.0")), "{e}");
        }
        assert!(target("0.5.0", &[], &set("0.6.0")).unwrap_err().contains("no changelog fragments"));
        // The command line.
        let a = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()).collect());
        assert_eq!(a(&["--set", "0.6.0"]), Ok(Some((false, set("0.6.0")))));
        assert_eq!(a(&["--dry-run", "--set=0.6.0"]), Ok(Some((true, set("0.6.0")))));
        for both in [&["--set", "0.6.0", "--release"][..], &["--release", "--set", "0.6.0"]] {
            assert!(a(both).unwrap_err().contains("--set and --release"), "{both:?}");
        }
        assert!(a(&["--set"]).unwrap_err().contains("--set needs a version"));
        assert!(a(&["--set", "--dry-run"]).unwrap_err().contains("not X.Y.Z"));
        for bad in ["v0.6.0", "0.6", "0.6.0.1", "0.6.x"] {
            assert!(a(&["--set", bad]).unwrap_err().contains("not X.Y.Z"), "{bad}");
        }
        assert!(a(&["--set", "0.6.0", "--set", "0.7.0"]).unwrap_err().contains("--set twice"));
    }

    #[test]
    fn version_set_folds_every_fragment_at_that_version() {
        let set = To::Set("0.6.0".into());
        let d = repo(
            "set",
            &[
                ("11.md", &frag("feature", "feature", "- feature eleven")),
                ("12.md", &frag("patch", "highlight", "- shines twelve")),
                ("13.none", ""),
            ],
        );
        add(&d, "14.md", &frag("patch", "fix", "- fix fourteen"));
        // The replay alone would make it 0.6.2.
        assert_eq!(run_in(&d, true, &To::Replay).unwrap(), "0.6.2");
        // A dry run returns (and prints) the set version and changes nothing.
        let head = sh(&d, &["rev-parse", "HEAD"]);
        assert_eq!(run_in(&d, true, &set).unwrap(), "0.6.0");
        assert_eq!(sh(&d, &["rev-parse", "HEAD"]), head);
        assert_eq!(sh(&d, &["status", "--porcelain"]), "");
        // Not above the current version: refused, nothing written.
        for low in ["0.5.0", "0.4.9"] {
            let e = run_in(&d, false, &To::Set(low.into())).unwrap_err();
            assert!(e.contains("is not above the current version 0.5.0"), "{e}");
            assert_eq!(sh(&d, &["rev-parse", "HEAD"]), head);
            assert_eq!(sh(&d, &["status", "--porcelain"]), "");
        }

        assert_eq!(run_in(&d, false, &set).unwrap(), "0.6.0");
        assert_eq!(sh(&d, &["rev-parse", "HEAD~1"]), head);
        assert_eq!(sh(&d, &["status", "--porcelain"]), "", "everything is in the one commit");
        assert_eq!(crate::release_ci::version_of(&d).unwrap(), "0.6.0");
        assert!(std::fs::read_to_string(d.join("Cargo.lock")).unwrap().contains("name = \"atlas\"\nversion = \"0.6.0\""));
        let cl = std::fs::read_to_string(d.join("CHANGELOG.md")).unwrap();
        assert_eq!(
            cl,
            "# Changelog\n\nHeader.\n\n## v0.6.0\n\n### Highlights\n\n- shines twelve\n\n### Features\n\n- feature eleven\n\n### Fixes\n\n- fix fourteen\n\n## v0.5.0\n\n### Highlights\n\n- old\n"
        );
        let names: Vec<String> = names_in(&d.join(DIR)).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["README.md"]);
        let msg = sh(&d, &["log", "-1", "--format=%B"]);
        assert!(msg.starts_with("Release v0.6.0") && msg.contains("0.5.0 -> 0.6.0 by --set (the fragments alone: 0.6.2)"), "{msg}");

        // The highlight refusal holds under --set too.
        let d = repo("set-nohl", &[("11.md", &frag("feature", "feature", "- f"))]);
        assert!(run_in(&d, false, &set).unwrap_err().contains("no Highlights"));
        assert_eq!(sh(&d, &["status", "--porcelain"]), "");
    }

    #[test]
    fn version_rolls_back_when_the_commit_fails() {
        let d = repo("rollback", &[("12.md", &frag("patch", "highlight", "- h"))]);
        let hook = d.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\necho refused by the test hook >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let head = sh(&d, &["rev-parse", "HEAD"]);
        let e = run_in(&d, false, &To::Replay).unwrap_err();
        assert!(e.contains("rolled back"), "{e}");
        assert_eq!(sh(&d, &["rev-parse", "HEAD"]), head);
        assert_eq!(sh(&d, &["status", "--porcelain"]), "", "nothing left half-written");
        assert!(d.join("changes/12.md").is_file());
        assert_eq!(crate::release_ci::version_of(&d).unwrap(), "0.5.0");
    }

    #[test]
    fn version_refusals() {
        // Only markers: nothing to release.
        let d = repo("none", &[("13.none", "")]);
        assert!(run_in(&d, false, &To::Replay).unwrap_err().contains("no changelog fragments"));
        // A malformed fragment and a stray name, both reported, nothing written.
        let d = repo("bad", &[("11.md", "---\nbump: huge\nsection: fix\n---\n- a\n"), ("notes.txt", "x"), ("12.md", &frag("patch", "highlight", "- h"))]);
        let e = run_in(&d, false, &To::Replay).unwrap_err();
        assert!(e.contains("changes/11.md: ") && e.contains("bump must be") && e.contains("changes/notes.txt: not a fragment name"), "{e}");
        assert_eq!(sh(&d, &["status", "--porcelain"]), "");
        // No highlight.
        let d = repo("nohl", &[("11.md", &frag("breaking", "breaking", "- b"))]);
        assert!(run_in(&d, false, &To::Replay).unwrap_err().contains("no Highlights"));
        assert_eq!(sh(&d, &["status", "--porcelain"]), "");
        // Uncommitted work, or a fragment not committed.
        let d = repo("dirty", &[("12.md", &frag("patch", "highlight", "- h"))]);
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = \"atlas\"\nversion = \"0.5.0\"\n# x\n").unwrap();
        assert!(run_in(&d, false, &To::Replay).unwrap_err().contains("uncommitted"));
        sh(&d, &["checkout", "-q", "--", "."]);
        std::fs::write(d.join("changes/14.md"), frag("breaking", "feature", "- f")).unwrap();
        assert!(run_in(&d, false, &To::Replay).unwrap_err().contains("not committed"));
        // Cargo.toml and plugin.json disagree.
        let d = repo("split", &[("12.md", &frag("patch", "highlight", "- h"))]);
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = \"atlas\"\nversion = \"0.5.1\"\n").unwrap();
        sh(&d, &["commit", "-qam", "drift"]);
        assert!(run_in(&d, false, &To::Replay).unwrap_err().contains("plugin.json version 0.5.0 is not Cargo.toml's 0.5.1"));
    }

    #[test]
    fn a_release_waits_for_pending_fragments() {
        let d = repo("pending", &[("13.none", "")]);
        refuse_pending(&d).unwrap();
        std::fs::write(d.join("changes/12.md"), frag("patch", "fix", "- f")).unwrap();
        assert!(refuse_pending(&d).unwrap_err().contains("cargo xtask version"));
        let empty = tmp("version-nodir");
        refuse_pending(&empty).unwrap();
    }

    #[test]
    fn stray_files_in_changes() {
        // A release waits only for fragment-form names: a stray file does
        // not hold it back, and a malformed fragment does.
        let d = repo("stray", &[("13.none", "")]);
        std::fs::write(d.join("changes/notes.txt"), "x").unwrap();
        std::fs::write(d.join("changes/.DS_Store"), "x").unwrap();
        std::fs::create_dir_all(d.join("changes/drafts")).unwrap();
        refuse_pending(&d).unwrap();
        std::fs::write(d.join("changes/21.md"), "not a fragment").unwrap();
        let e = refuse_pending(&d).unwrap_err();
        assert!(e.contains("21.md") && e.contains("cargo xtask version"), "{e}");
        // `cargo xtask version` holds the folder to git's own list: a
        // fragment git ignores is refused, not folded and left behind.
        let d = repo("ignored", &[("12.md", &frag("patch", "highlight", "- h"))]);
        std::fs::write(d.join(".gitignore"), "changes/2*.md\n").unwrap();
        sh(&d, &["add", ".gitignore"]);
        sh(&d, &["commit", "-q", "-m", "ignore"]);
        std::fs::write(d.join("changes/20.md"), frag("feature", "feature", "- f")).unwrap();
        let head = sh(&d, &["rev-parse", "HEAD"]);
        let e = run_in(&d, false, &To::Replay).unwrap_err();
        assert!(e.contains("changes/20.md") && e.contains("not tracked"), "{e}");
        assert_eq!(sh(&d, &["rev-parse", "HEAD"]), head);
        assert_eq!(sh(&d, &["status", "--porcelain"]), "");
        assert_eq!(crate::release_ci::version_of(&d).unwrap(), "0.5.0");
    }

    /// This repository's own fragments parse.
    #[test]
    fn the_repository_fragments_are_well_formed() {
        let p = pending(&repo_root().join(DIR)).unwrap();
        assert!(p.files.len() >= p.fragments.len());
    }

    // ------------------------------------------------ scripts/changes-check.sh

    /// Runs the dev-pr check for pull request `pr` over `listing` (the
    /// files API's `status<TAB>path` lines), with `files` written under a
    /// fresh root; its exit status and output.
    fn check(name: &str, pr: &str, build: bool, listing: &[(&str, &str)], files: &[(&str, &[u8])]) -> (bool, String) {
        let sh_exe = posix_sh().expect("a POSIX sh (Git for Windows' on Windows)");
        let root = tmp(&format!("changes-check-{name}"));
        for (f, t) in files {
            std::fs::create_dir_all(root.join(f).parent().unwrap()).unwrap();
            std::fs::write(root.join(f), t).unwrap();
        }
        let list: String = listing.iter().map(|(s, p)| format!("{s}\t{p}\n")).collect();
        let script = repo_root().join("scripts/changes-check.sh");
        let mut c = Command::new(&sh_exe);
        if let Some(dir) = sh_exe.parent().filter(|d| !d.as_os_str().is_empty()) {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let joined = std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&path))).unwrap();
            c.env("PATH", joined);
        }
        let mut child = c
            .arg(&script)
            .arg(pr)
            .arg(if build { "true" } else { "false" })
            .arg(&root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child.stdin.take().unwrap().write_all(list.as_bytes()).unwrap();
        let o = child.wait_with_output().unwrap();
        (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    }

    #[test]
    fn dev_pr_check_wants_a_fragment_or_a_marker_on_a_build_path() {
        let good = frag("feature", "feature", "- f");
        let good = good.as_bytes();
        let src = ("modified", "src/lib.rs");
        // A fragment.
        let (ok, out) = check("frag", "187", true, &[src, ("added", "changes/187.md")], &[("changes/187.md", good)]);
        assert!(ok, "{out}");
        let (ok, out) = check("frag2", "187", true, &[src, ("added", "changes/187-more.md")], &[("changes/187-more.md", good)]);
        assert!(ok, "{out}");
        // A marker.
        let (ok, out) = check("none", "187", true, &[src, ("added", "changes/187.none")], &[("changes/187.none", b"")]);
        assert!(ok, "{out}");
        // Neither.
        let (ok, out) = check("neither", "187", true, &[src, ("modified", "README.md")], &[]);
        assert!(!ok && out.contains("::error::") && out.contains("changes/187.md") && out.contains("changes/187.none"), "{out}");
        // Another pull request's fragment does not count.
        let (ok, out) = check("other", "187", true, &[src, ("added", "changes/186.md")], &[("changes/186.md", good)]);
        assert!(!ok, "{out}");
        // A removed fragment does not count either...
        let (ok, out) = check("rm-own", "187", true, &[src, ("removed", "changes/187.md")], &[]);
        assert!(!ok, "{out}");
        // ...except as a `cargo xtask version` fold, which removes others'
        // fragments and writes CHANGELOG.md and Cargo.toml.
        let (ok, out) = check("fold", "190", true, &[("modified", "Cargo.toml"), ("modified", "CHANGELOG.md"), ("removed", "changes/187.md"), ("removed", "changes/188.none")], &[]);
        assert!(ok, "{out}");
        // Deleting another pull request's fragment is not a fold by itself.
        let (ok, out) = check("bypass", "191", true, &[src, ("removed", "changes/187.md")], &[]);
        assert!(!ok && out.contains("changes/191.md"), "{out}");
        let (ok, out) = check("bypass2", "191", true, &[src, ("modified", "CHANGELOG.md"), ("removed", "changes/187.md")], &[]);
        assert!(!ok && out.contains("changes/191.md"), "{out}");
        // No build path: nothing is required.
        let (ok, out) = check("docs", "187", false, &[("modified", "wiki/Home.md")], &[]);
        assert!(ok, "{out}");
        // Both a fragment and a marker.
        let (ok, out) = check("both", "187", true, &[src, ("added", "changes/187.md"), ("added", "changes/187.none")], &[("changes/187.md", good), ("changes/187.none", b"")]);
        assert!(!ok && out.contains("187.none"), "{out}");
        // A bad name, even on a docs-only pull request.
        let (ok, out) = check("badname", "187", false, &[("added", "changes/notes.txt")], &[("changes/notes.txt", b"x")]);
        assert!(!ok && out.contains("not a fragment name"), "{out}");
        // A malformed fragment of its own, even with no build path.
        let (ok, out) = check("bad", "187", false, &[("added", "changes/187.md")], &[("changes/187.md", b"---\nbump: huge\nsection: fix\n---\n- a\n")]);
        assert!(!ok && out.contains("bump must be"), "{out}");
        // A byte order mark, and a file that is not UTF-8, each named.
        let bom = "\u{feff}---\nbump: patch\nsection: fix\n---\n- a\n";
        let (ok, out) = check("bom", "187", false, &[("added", "changes/187.md")], &[("changes/187.md", bom.as_bytes())]);
        assert!(!ok && out.contains("byte order mark"), "{out}");
        let (ok, out) = check("latin1", "187", false, &[("added", "changes/187.md")], &[("changes/187.md", BAD_BYTES[0].0)]);
        assert!(!ok && out.contains("UTF-8"), "{out}");
    }

    /// A path from the pull request's listing reaches a workflow command only
    /// with `%`, CR and LF taken out and `:` and `,` made `_`, so it cannot
    /// end the command, start another, or end the `file` property and add
    /// one.
    #[test]
    fn dev_pr_check_cleans_paths_in_workflow_commands() {
        let (ok, out) = check("inject", "187", false, &[("added", "changes/x%0A::warning::y%25\r,line=1.txt")], &[]);
        let clean = "changes/x0A__warning__y25_line=1.txt";
        assert!(!ok && out.contains(&format!("::error file={clean}::{clean}: not a fragment name")), "{out:?}");
        assert!(!out.contains('%') && !out.contains('\r') && !out.contains("::warning"), "{out:?}");
        let line = out.lines().find(|l| l.starts_with("::error file=")).unwrap();
        assert_eq!(line.matches("::").count(), 2, "{line:?}");
        assert!(!line.split("::").nth(1).unwrap().contains(','), "one property: {line:?}");
    }

    /// The script and [`parse_fragment_bytes`] agree on every fragment here.
    #[test]
    fn dev_pr_check_and_the_parser_agree() {
        let mut texts: Vec<Vec<u8>> = BAD.iter().map(|(t, _)| t.as_bytes().to_vec()).collect();
        texts.extend(BAD_BYTES.iter().map(|(b, _)| b.to_vec()));
        texts.extend(
            [
                frag("patch", "fix", "- a"),
                frag("breaking", "breaking", "- a"),
                frag("feature", "feature", "- caf\u{e9}, in UTF-8"),
                "---\r\nbump: feature\r\nsection: known-limit\r\n---\r\n- a\r\n".to_string(),
                "---\n\nsection :  upgrade\nbump:patch\n---\n\n- a\n  ```\n  # x\n  ```\n".to_string(),
                "---\nbump: feature\nsection: highlight\n---\n- **Bold.** text, <kbd>x</kbd> and a < b\n".to_string(),
                "---\nbump: patch\nsection: fix\n---\n- a\n   ~~~\n   ## v0.0.1\n    <!--\n\n   ~~~~ \n".to_string(),
                "---\nbump: patch\nsection: fix\n---\n- a\n  ````sh\n  # x\n  ```\n  ````\n".to_string(),
                "---\nbump: patch\nsection: fix\n---\n- a\n  ~~~ `x`\n  # x\n\t<b>\n  ~~~\n".to_string(),
                "---\nbump: patch\nsection: fix\n---\n- a\n```\n<details>\n## v0.0.1\n```\n- b\n".to_string(),
                // Tabs, `#` that opens no heading, and lines of = or - in a fence.
                "---\nbump: patch\nsection: fix\n---\n- a\tb, fixes #123\n  #5 and C# too\n- #7\n  ####### x\n  a -- b = c\n  ```\n  ===\n  ---\n  - # x\n  ```\n".to_string(),
            ]
            .map(String::into_bytes),
        );
        for (i, t) in texts.iter().enumerate() {
            let rust = parse_fragment_bytes("9.md", t).is_ok();
            let (script, out) = check(&format!("agree-{i}"), "9", true, &[("added", "changes/9.md")], &[("changes/9.md", t)]);
            assert_eq!(script, rust, "{:?}: script says {out}", String::from_utf8_lossy(t));
            // Agreeing is not enough: every BAD case is refused by both.
            assert_eq!(rust, i >= BAD.len() + BAD_BYTES.len(), "{:?}: script says {out}", String::from_utf8_lossy(t));
        }
    }

    /// dev-pr's `changes` job, checked to run the fragment check cheaply (a
    /// sparse checkout and a shell step); `None` where there is no dev-pr
    /// workflow: the public repository, which the export leaves it out of.
    fn dev_pr_changes_job(root: &Path) -> Option<String> {
        let wf = root.join(".github/workflows/dev-pr.yml");
        if !wf.is_file() {
            return None;
        }
        let wf = read_text(&wf).unwrap();
        let job = wf[wf.find("  changes:").unwrap()..wf.find("  release-check:").unwrap()].to_string();
        assert!(job.contains("sh scripts/changes-check.sh \"$PR\" \"$BUILD\" ."), "{job}");
        assert!(job.contains("sparse-checkout"), "{job}");
        for build in ["cargo build", "cargo test", "cargo +", "cargo xtask", "rustup"] {
            assert!(!job.contains(build), "no build in the changes job: {build}");
        }
        Some(job)
    }

    #[test]
    fn dev_pr_runs_the_check_in_its_changes_job() {
        let root = repo_root();
        match dev_pr_changes_job(&root) {
            Some(_) => {}
            // export-ignore keeps dev-pr.yml out of the public repository.
            None => eprintln!("skipped: no dev-pr workflow (the public repository)"),
        }
    }

    #[test]
    fn a_public_export_without_dev_pr_passes() {
        let root = tmp("version-public-layout");
        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        std::fs::write(root.join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
        std::fs::write(root.join(".github/workflows/release.yml"), "name: release\n").unwrap();
        assert_eq!(dev_pr_changes_job(&root), None);
    }
}
