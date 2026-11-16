//! Required reviews before a merge (`orchestrate.reviews`).
//!
//! A builder's PR needs a review from each configured role (job names such
//! as `simplicity` and `cost`, never persons) of the head it will merge at.
//! A review is a lane journal entry: kind `review` with meta `role:`,
//! `pr:`, `head:`, `verdict:` and `report:` lines, or a `note` with a
//! `review: ROLES` meta line (the form reviews were filed in before the
//! kind existed: `pr:` and `head:` may then list several, and `verdict:`
//! may name a role in parentheses, `request-changes (simplicity)`).
//!
//! The PreToolUse hook reads each `gh pr merge` in a shell command, asks
//! `gh pr view` for the PR's number and head sha (Q-RV1), and names every
//! role whose review of that head is missing. A `request changes` verdict
//! on that head blocks its role (Q-RV3): only a review of a newer head that
//! passes clears it. Enforced mode denies the command, advisory mode adds
//! the line as context; with no roles the gate is off.
//!
//! See `docs/design/reviews.md`.

use std::process::Command;

use crate::json::Value;
use crate::memory::MemoryStore;
use crate::orchestrate::Mode;
use crate::queue::meta_value;
use crate::Result;

/// The journal entry kind a review is filed as.
pub const ENTRY: &str = "review";

/// The resolved `orchestrate.reviews` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSettings {
    /// The roles every PR needs, lowercased. None: the gate is off.
    pub roles: Vec<String>,
    /// Enforced denies the merge, advisory warns, off does nothing.
    pub mode: Mode,
}

impl ReviewSettings {
    /// Whether the gate runs.
    pub fn on(&self) -> bool {
        !self.roles.is_empty() && self.mode != Mode::Off
    }
}

/// A repo as the gate compares it: lowercase `owner/repo` (or `repo` when
/// only a name is given). A scheme, a host (`github.com/`,
/// `git@github.com:`), a trailing `.git` or `/` and a PR URL's `/pull/N`
/// tail are dropped. `None` when empty, or when it lists several (a comma).
pub fn repo_name(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_lowercase();
    if s.contains(',') {
        return None;
    }
    let s = s.split_once("://").map_or(s.as_str(), |(_, r)| r);
    let s = s.split("/pull/").next().unwrap_or("");
    // `git@host:owner/repo`: the host ends at the colon.
    let s = s.split_once('@').map_or(s, |(_, r)| r).replacen(':', "/", 1);
    let parts: Vec<&str> = s.split('/').map(str::trim).filter(|p| !p.is_empty()).collect();
    let name = parts[parts.len().saturating_sub(2)..].join("/");
    let name = name.strip_suffix(".git").unwrap_or(&name);
    Some(name.to_string()).filter(|n| !n.is_empty())
}

/// The repos a free-text `repo:` line names (`api, react`), each read by
/// [`repo_name`].
fn repo_names(line: &str) -> Vec<String> {
    line.split(',').filter_map(repo_name).collect()
}

/// A PR as a review or a merge names it: its number and, when named, its
/// repo (`repo` or `owner/repo`, lowercased). An unqualified PR (every
/// review filed before #183) names no repo and matches that number in any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    /// The repo, when named.
    pub repo: Option<String>,
    /// The PR number.
    pub number: u64,
}

impl Pr {
    /// Reads `N`, `#N`, `repo#N`, `owner/repo#N` or a PR's URL, with the
    /// repo in any form [`repo_name`] reads; `repo` (a `repo:` meta line or
    /// a `--repo`) is the repo of one that names none.
    pub fn parse(s: &str, repo: Option<&str>) -> Option<Pr> {
        let s = s.trim();
        let (r, n) = match s.split_once("/pull/") {
            Some((base, tail)) => (base, tail.split(['/', '#', '?']).next().unwrap_or("")),
            None => s.rsplit_once('#').unwrap_or(("", s)),
        };
        let number = n.trim().parse().ok()?;
        Some(Pr { repo: repo_name(r).or_else(|| repo.and_then(repo_name)), number })
    }

    /// Whether two name the same PR: the same number, and the same repo when
    /// both name one (the owner counts only when both give it).
    pub fn same(&self, o: &Pr) -> bool {
        let name = |r: &str| r.rsplit('/').next().unwrap_or("").to_string();
        self.number == o.number
            && match (&self.repo, &o.repo) {
                (Some(a), Some(b)) if a.contains('/') && b.contains('/') => a == b,
                (Some(a), Some(b)) => name(a) == name(b),
                _ => true,
            }
    }
}

impl std::fmt::Display for Pr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.repo.as_deref().unwrap_or(""), self.number)
    }
}

/// One filed review, read from its meta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// The roles it speaks for, lowercased.
    pub roles: Vec<String>,
    /// The PRs it read.
    pub prs: Vec<Pr>,
    /// The heads it read (7 or more hex digits, lowercased).
    pub heads: Vec<String>,
    /// Each verdict part: the role it names (`None`: every role), and
    /// whether it passes.
    pub verdicts: Vec<(Option<String>, bool)>,
}

/// The comma-separated items of a meta value, trimmed, lowercased.
fn items(v: &str) -> impl Iterator<Item = String> + '_ {
    v.split(',').map(|x| x.trim().to_ascii_lowercase()).filter(|x| !x.is_empty())
}

/// Whether a verdict passes: everything does but a request for changes.
/// This loose read is for legacy notes; a `review` entry is held to
/// [`VERDICTS`] when it is filed.
fn passes(verdict: &str) -> bool {
    let v = verdict.to_ascii_lowercase();
    !(v.contains("request") && v.contains("change"))
}

/// The verdicts a `review` entry may give, as [`verdict_words`] reads them.
pub const VERDICTS: &[&str] = &["pass", "merge", "merge with follow ups", "request changes"];

/// A verdict part without its `(role)`, lowercased, with `-` and `_` read
/// as spaces and runs of spaces as one.
fn verdict_words(part: &str) -> String {
    let bare = match (part.rfind('('), part.trim_end().ends_with(')')) {
        (Some(i), true) => &part[..i],
        _ => part,
    };
    bare.to_ascii_lowercase().replace(['-', '_'], " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether every verdict part of a review's meta is one of [`VERDICTS`]:
/// the filing check for `review` entries, so an unknown word such as
/// `reject` or `do not merge` is refused rather than read as a pass.
pub fn verdicts_known(meta: &str) -> bool {
    meta_value(meta, "verdict").is_some_and(|v| items(v).all(|part| VERDICTS.contains(&verdict_words(&part).as_str())))
}

impl Review {
    /// Reads a review from its meta: `role:` (or `review:`), `pr:`,
    /// `head:` and `verdict:` lines, each required, and an optional `repo:`
    /// for the PRs that name none. `None` when one is missing or names
    /// nothing usable ([`Review::problem`] says which).
    pub fn parse(meta: &str) -> Option<Review> {
        let roles: Vec<String> = items(meta_value(meta, "role").or_else(|| meta_value(meta, "review"))?).collect();
        // A `repo:` line names the repo of each bare `pr:`; an old free-text
        // one that lists several (`api, react`) names each.
        let repos = meta_value(meta, "repo").map(repo_names).unwrap_or_default();
        let with: Vec<Option<&str>> = if repos.is_empty() { vec![None] } else { repos.iter().map(|r| Some(r.as_str())).collect() };
        let mut prs: Vec<Pr> = items(meta_value(meta, "pr")?).flat_map(|p| with.iter().filter_map(|r| Pr::parse(&p, *r)).collect::<Vec<_>>()).collect();
        prs.dedup();
        let heads: Vec<String> = items(meta_value(meta, "head")?).filter(|h| h.len() >= 7 && h.bytes().all(|b| b.is_ascii_hexdigit())).collect();
        let verdicts: Vec<(Option<String>, bool)> = items(meta_value(meta, "verdict")?)
            .map(|part| match (part.rfind('('), part.ends_with(')')) {
                (Some(i), true) => {
                    let who = part[i + 1..part.len() - 1].trim().to_string();
                    (roles.contains(&who).then_some(who), passes(&part[..i]))
                }
                _ => (None, passes(&part)),
            })
            .collect();
        if roles.is_empty() || prs.is_empty() || heads.is_empty() || verdicts.is_empty() {
            return None;
        }
        Some(Review { roles, prs, heads, verdicts })
    }

    /// The first meta line a review entry lacks or cannot use, named for
    /// the refusal; `None` when [`Review::parse`] reads it.
    pub fn problem(meta: &str) -> Option<&'static str> {
        if Review::parse(meta).is_some() {
            return None;
        }
        let has = |k: &str, ok: &dyn Fn(&str) -> bool| meta_value(meta, k).is_some_and(|v| items(v).any(|x| ok(&x)));
        Some(if !has("role", &|_| true) && !has("review", &|_| true) {
            "`role:` is missing"
        } else if !has("pr", &|p| Pr::parse(p, None).is_some()) {
            "`pr:` names no PR (it takes N, #N, repo#N or owner/repo#N)"
        } else if !has("head", &|h| h.len() >= 7 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            "`head:` names no sha of 7 or more hex digits"
        } else {
            "`verdict:` is missing"
        })
    }

    /// Whether it read this PR at this head (a listed head is a prefix of
    /// the full sha).
    pub fn reads(&self, pr: &Pr, head: &str) -> bool {
        self.prs.iter().any(|p| p.same(pr)) && self.heads.iter().any(|h| head.starts_with(h.as_str()))
    }

    /// Its verdict for one role: the parts that name the role, else the
    /// parts that name none; it passes when every one of them does.
    pub fn verdict(&self, role: &str) -> Option<bool> {
        if !self.roles.iter().any(|r| r == role) {
            return None;
        }
        let own: Vec<bool> = self.verdicts.iter().filter(|(w, _)| w.as_deref() == Some(role)).map(|(_, p)| *p).collect();
        let pick: Vec<bool> = if own.is_empty() { self.verdicts.iter().filter(|(w, _)| w.is_none()).map(|(_, p)| *p).collect() } else { own };
        (!pick.is_empty()).then(|| pick.iter().all(|p| *p))
    }
}

/// Every active review of a PR, oldest first.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn reviews(store: &MemoryStore, pr: &Pr) -> Result<Vec<Review>> {
    Ok(store.review_metas()?.into_iter().filter_map(|(_, m)| Review::parse(&m)).filter(|r| r.prs.iter().any(|p| p.same(pr))).collect())
}

/// What each role still needs before PR `pr` merges at `head`: empty when
/// every role has a passing review of that head and none asks for
/// changes on it.
pub fn missing(roles: &[String], reviews: &[Review], pr: &Pr, head: &str) -> Vec<String> {
    let short = head.get(..7).unwrap_or(head);
    roles
        .iter()
        .filter_map(|role| {
            let at_head: Vec<bool> = reviews.iter().filter(|r| r.reads(pr, head)).filter_map(|r| r.verdict(role)).collect();
            if at_head.contains(&false) {
                Some(format!("{role} review requested changes at head {short} (a review of a newer head must pass)"))
            } else if at_head.is_empty() {
                Some(format!("{role} review for head {short}"))
            } else {
                None
            }
        })
        .collect()
}

/// One `gh pr merge` in a shell command: what it names, as `gh pr view`
/// takes it (a number, URL or branch; none for the current branch's PR),
/// and its `--repo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Merge {
    /// The PR selector.
    pub selector: Option<String>,
    /// The `--repo` / `-R` value.
    pub repo: Option<String>,
}

/// A shell command as words, quotes removed; a command separator (`;`,
/// `&`, `|`, a newline) is an empty word. An unquoted `#` that starts a word
/// starts a comment to the end of the line, in bash and PowerShell alike: in
/// `gh pr merge #99` the shell runs `gh pr merge`, which merges the current
/// branch's PR, not #99.
fn words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut comment = false;
    for c in command.chars() {
        if comment {
            if c != '\n' {
                continue;
            }
            comment = false;
        }
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '#') if cur.is_empty() && !started => comment = true,
            // `2>&1` and `<&3` are one redirection word, not a separator.
            (None, '&') if cur.ends_with(['>', '<']) => cur.push(c),
            (None, ';' | '&' | '|' | '\n' | '(' | ')') => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                started = false;
                out.push(String::new());
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                started = false;
            }
            (None, c) => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The `gh pr merge` flags that take a value.
const VALUE_FLAGS: &[&str] = &["-A", "--author-email", "-b", "--body", "-F", "--body-file", "--match-head-commit", "-R", "--repo", "-t", "--subject"];

/// Whether a word is a redirection (`>`, `2>&1`, `>>out.txt`, `<in`, and
/// PowerShell's `*>&1`), and if so whether its target is the next word (`>`,
/// `2>` and `*>` alone).
fn redirection(word: &str) -> Option<bool> {
    let rest = word.trim_start_matches(|c: char| c.is_ascii_digit() || c == '*');
    rest.starts_with(['>', '<']).then(|| rest.trim_start_matches(['>', '<', '&']).is_empty())
}

/// Every `gh pr merge` in a shell command.
pub fn merges(command: &str) -> Vec<Merge> {
    if !command.contains("merge") {
        return Vec::new();
    }
    let w = words(command);
    let is_gh = |s: &str| {
        let base = s.rsplit(['/', '\\']).next().unwrap_or(s).to_ascii_lowercase();
        base == "gh" || base == "gh.exe"
    };
    let mut out = Vec::new();
    for i in 0..w.len() {
        if !(is_gh(&w[i]) && w.get(i + 1).is_some_and(|x| x == "pr") && w.get(i + 2).is_some_and(|x| x == "merge")) {
            continue;
        }
        let mut m = Merge::default();
        let mut j = i + 3;
        while let Some(a) = w.get(j) {
            if a.is_empty() {
                break;
            }
            if let Some(next_is_target) = redirection(a) {
                j += usize::from(next_is_target);
            } else if let Some(r) = a.strip_prefix("--repo=").or_else(|| a.strip_prefix("-R=")) {
                m.repo = Some(r.to_string());
            } else if VALUE_FLAGS.contains(&a.as_str()) {
                if a == "-R" || a == "--repo" {
                    m.repo = w.get(j + 1).cloned();
                }
                j += 1;
            } else if !a.starts_with('-') && m.selector.is_none() {
                m.selector = Some(a.clone());
            }
            j += 1;
        }
        out.push(m);
    }
    out
}

/// A PR (with its repo when gh names it) and its head sha, or why they
/// could not be read.
pub type HeadResult = std::result::Result<(Pr, String), String>;

/// How long the gate waits for `gh pr view`.
const GH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The PR's number, repo and head sha from `gh pr view --json
/// number,headRefOid,url`, run in `cwd`.
///
/// # Errors
///
/// A message when `gh` cannot run or answers without both.
pub fn gh_head(m: &Merge, cwd: &str) -> HeadResult {
    let mut c = Command::new("gh");
    c.args(["pr", "view"]);
    if let Some(s) = &m.selector {
        c.arg(s);
    }
    if let Some(r) = &m.repo {
        c.args(["--repo", r]);
    }
    c.args(["--json", "number,headRefOid,url"]);
    if !cwd.is_empty() && std::path::Path::new(cwd).is_dir() {
        c.current_dir(cwd);
    }
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    crate::code_index::no_window(&mut c);
    let child = c.spawn().map_err(|e| format!("gh: {e}"))?;
    let pid = child.id();
    // The waiter owns the child; the deadline is a channel receive, so a
    // hung gh costs GH_TIMEOUT rather than the harness's hook timeout.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = match rx.recv_timeout(GH_TIMEOUT) {
        Ok(r) => r.map_err(|e| format!("gh: {e}"))?,
        Err(_) => {
            crate::platform::process::terminate(pid);
            return Err(format!("gh pr view: no answer in {} s", GH_TIMEOUT.as_secs()));
        }
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("gh pr view: {}", err.lines().next().unwrap_or("failed").trim()));
    }
    let v = crate::json::parse(&String::from_utf8_lossy(&out.stdout)).map_err(|e| format!("gh pr view: {e}"))?;
    let number = v.get("number").and_then(Value::as_u64);
    let head = v.get("headRefOid").and_then(Value::as_str).map(str::to_ascii_lowercase);
    // `https://github.com/OWNER/REPO/pull/N`: the repo the PR is in.
    let repo = v.get("url").and_then(Value::as_str).and_then(repo_name);
    match (number, head) {
        (Some(n), Some(h)) if h.len() >= 7 => Ok((Pr { repo: repo.or_else(|| m.repo.as_deref().and_then(repo_name)), number: n }, h)),
        _ => Err("gh pr view: no number and headRefOid in its answer".into()),
    }
}

/// The gate's answer for one shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    /// Deny the command (enforced), rather than warn (advisory).
    pub deny: bool,
    /// The line to show.
    pub message: String,
}

/// Checks every `gh pr merge` in `command`. `None` when the gate is off,
/// the command merges nothing, or every merge has its reviews. A PR whose
/// head cannot be read is reported, since its reviews cannot be checked.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn gate(store: &MemoryStore, s: &ReviewSettings, command: &str, head_of: &dyn Fn(&Merge) -> HeadResult) -> Result<Option<Gate>> {
    if !s.on() {
        return Ok(None);
    }
    let mut lines = Vec::new();
    for m in merges(command) {
        // A numbered PR that some role has no review of at all is missing
        // that review whatever its head: say so without asking gh (about
        // half a second), the likeliest denial. The reviews read here are
        // kept for the head check when gh names the same PR.
        // A `--repo` names its repo; without one, every repo's reviews of
        // the number count here, and gh's answer narrows them below.
        if let Some(r) = m.repo.as_deref().filter(|r| r.contains(',')) {
            lines.push(format!("--repo {r} names several repos; name one"));
            continue;
        }
        let mut read: Option<(Pr, Vec<Review>)> = None;
        if let Some(number) = m.selector.as_deref().and_then(|x| x.trim_start_matches('#').parse::<u64>().ok()) {
            let pr = Pr { repo: m.repo.as_deref().and_then(repo_name), number };
            let rs = reviews(store, &pr)?;
            let unread: Vec<String> = s.roles.iter().filter(|role| !rs.iter().any(|r| r.verdict(role).is_some())).map(|role| format!("{role} review")).collect();
            if !unread.is_empty() {
                lines.push(format!("PR {pr} is missing: {}", unread.join("; ")));
                continue;
            }
            read = Some((pr, rs));
        }
        match head_of(&m) {
            Err(e) => lines.push(format!("the PR {} merges cannot be checked ({e})", m.selector.as_deref().unwrap_or("this branch"))),
            Ok((pr, head)) => {
                // Reviews read for the same PR (or for its number in any
                // repo) are a superset; `missing` keeps this repo's.
                let rs = match read {
                    Some((p, rs)) if p.same(&pr) => rs,
                    _ => reviews(store, &pr)?,
                };
                let need = missing(&s.roles, &rs, &pr, &head);
                if !need.is_empty() {
                    lines.push(format!("PR {pr} is missing: {}", need.join("; ")));
                }
            }
        }
    }
    if lines.is_empty() {
        return Ok(None);
    }
    let deny = s.mode == Mode::Enforced;
    let (last, rest) = VERDICTS.split_last().unwrap_or((&"", &[]));
    let message = format!(
        "atlas reviews: {}{}. File each review as a journal entry: entry `review`, meta lines `role:`, `pr:` (N or repo#N), `head:` (the sha it read), `verdict:` ({} or {last}) and `report:`.",
        if deny { "merge denied. " } else { "do not merge yet. " },
        lines.join(". "),
        rest.join(", ")
    );
    Ok(Some(Gate { deny, message }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_are_read_with_their_selector_and_repo() {
        assert_eq!(merges("gh pr merge 99 --squash"), [Merge { selector: Some("99".into()), repo: None }]);
        assert_eq!(merges("cd x && gh pr merge --repo o/r -t \"a title with 5 words\" 12 && echo ok"), [Merge { selector: Some("12".into()), repo: Some("o/r".into()) }]);
        assert_eq!(merges("gh pr merge --merge"), [Merge::default()]);
        assert_eq!(merges("\"C:/Program Files/GitHub CLI/gh.exe\" pr merge https://github.com/o/r/pull/7 -R=x"), [Merge { selector: Some("https://github.com/o/r/pull/7".into()), repo: Some("x".into()) }]);
        // A redirection is never the selector, with its target apart or not.
        assert_eq!(merges("gh pr merge --squash 2>&1"), [Merge::default()]);
        assert_eq!(merges("gh pr merge > out.txt"), [Merge::default()]);
        assert_eq!(merges("gh pr merge 2> err.txt 12 >>log"), [Merge { selector: Some("12".into()), repo: None }]);
        assert_eq!(merges("gh pr merge 1; gh pr merge 2").len(), 2);
        // PowerShell's all-streams redirection.
        assert_eq!(merges("gh pr merge --squash *>&1"), [Merge::default()]);
        assert_eq!(merges("gh pr merge *> out.txt 12"), [Merge { selector: Some("12".into()), repo: None }]);
        // `#99` is a comment to the shell: gh merges the current branch's PR.
        assert_eq!(merges("gh pr merge #99 --squash"), [Merge::default()]);
        assert_eq!(merges("gh pr merge # 99\ngh pr merge 7"), [Merge::default(), Merge { selector: Some("7".into()), repo: None }]);
        // Inside a word or quotes, `#` is text.
        assert_eq!(merges("gh pr merge https://github.com/o/r/pull/7#x"), [Merge { selector: Some("https://github.com/o/r/pull/7#x".into()), repo: None }]);
        assert_eq!(merges("gh pr merge '#5'"), [Merge { selector: Some("#5".into()), repo: None }]);
        assert!(merges("gh pr view 99; git merge main; echo 'gh pr merge 3'").is_empty());
    }

    #[test]
    fn both_review_forms_are_read() {
        let r = Review::parse("role: cost\npr: 99\nhead: 6E828CC\nverdict: merge with follow-ups\nreport: x.md").unwrap();
        assert_eq!((r.roles.as_slice(), r.prs.as_slice(), r.heads.as_slice()), (&["cost".to_string()][..], &[pr(99)][..], &["6e828cc".to_string()][..]));
        assert_eq!(r.verdict("cost"), Some(true));
        assert_eq!(r.verdict("simplicity"), None);
        // A note as filed before the kind existed: one verdict per role.
        let n = Review::parse("review: simplicity, cost\npr: 99\nhead: d84126e\nverdict: request-changes (simplicity), merge-with-follow-ups (cost)\n").unwrap();
        assert_eq!((n.verdict("simplicity"), n.verdict("cost")), (Some(false), Some(true)));
        // A parenthesis that names no role applies to every role.
        let b = Review::parse("review: simplicity, cost\npr: 368, 371\nhead: 9028fabd, b0af1526\nverdict: pass-with-fixes (both)").unwrap();
        assert!(b.reads(&pr(371), "b0af1526ffff") && b.reads(&pr(368), "9028fabd00") && !b.reads(&pr(370), "9028fabd00"));
        assert_eq!(b.verdict("cost"), Some(true));
        // A review entry gives one of the known verdicts; legacy notes are
        // read loosely.
        assert!(verdicts_known("verdict: MERGE-WITH-FOLLOW-UPS"));
        assert!(verdicts_known("verdict: request-changes (simplicity), pass (cost)"));
        for bad in ["reject", "block", "fail", "do not merge", "pass, reject"] {
            assert!(!verdicts_known(&format!("verdict: {bad}")), "{bad}");
        }
        assert!(!verdicts_known("role: cost"));
        // Without a head or a verdict it is no review.
        assert!(Review::parse("role: cost\npr: 9\nverdict: pass").is_none());
        assert!(Review::parse("role: cost\npr: 9\nhead: abc\nverdict: pass").is_none());
    }

    fn pr(n: u64) -> Pr {
        Pr { repo: None, number: n }
    }

    fn pr_in(repo: &str, n: u64) -> Pr {
        Pr { repo: Some(repo.into()), number: n }
    }

    /// #183: `pr:` takes `N`, `#N`, `repo#N` and `owner/repo#N`.
    #[test]
    fn repo_qualified_prs_are_read() {
        for pr in ["386", "#386", "es-modern-contracts#386", "bwhite/es-modern-contracts#386", "Bwhite/ES-Modern-Contracts#386"] {
            let r = Review::parse(&format!("role: taste\npr: {pr}\nhead: b4baa646\nverdict: merge with follow-ups")).unwrap_or_else(|| panic!("{pr}"));
            assert_eq!(r.verdict("taste"), Some(true), "{pr}");
        }
        assert!(Review::parse("role: taste\npr: es-modern-contracts#\nhead: b4baa646\nverdict: pass").is_none());
        assert!(Review::parse("role: taste\npr: es-modern-contracts\nhead: b4baa646\nverdict: pass").is_none());
        // The refusal names the field.
        assert_eq!(Review::problem("role: taste\npr: es-modern-contracts\nhead: b4baa646\nverdict: pass"), Some("`pr:` names no PR (it takes N, #N, repo#N or owner/repo#N)"));
        assert_eq!(Review::problem("role: taste\npr: 3\nhead: b4\nverdict: pass"), Some("`head:` names no sha of 7 or more hex digits"));
        assert_eq!(Review::problem("role: taste\npr: a#3\nhead: b4baa646\nverdict: pass"), None);
    }

    /// #183: PRs are keyed by repo and number, so one number in two repos
    /// does not collide; an unqualified one (an old record) still reads.
    #[test]
    fn one_pr_number_in_two_repos_does_not_collide() {
        let head = "b4baa646ffff";
        let api = Review::parse("role: cost\npr: api#784\nhead: b4baa64\nverdict: pass").unwrap();
        let react = Review::parse("role: cost\npr: o/react#784\nhead: b4baa64\nverdict: request changes").unwrap();
        // A `repo:` line names the repo of a bare number.
        let by_line = Review::parse("role: cost\npr: 784\nrepo: React\nhead: b4baa64\nverdict: request changes").unwrap();
        assert_eq!(by_line.prs, [pr_in("react", 784)]);
        let old = Review::parse("role: cost\npr: 784\nhead: b4baa64\nverdict: pass").unwrap();
        assert!(api.reads(&pr_in("o/api", 784), head) && !api.reads(&pr_in("o/react", 784), head) && !api.reads(&pr_in("api", 785), head));
        assert!(react.reads(&pr_in("react", 784), head) && !react.reads(&pr_in("other/react", 784), head) && react.reads(&pr(784), head));
        assert!(old.reads(&pr_in("o/api", 784), head) && old.reads(&pr_in("o/react", 784), head));
        let roles = vec!["cost".to_string()];
        assert!(missing(&roles, &[api.clone(), react.clone()], &pr_in("o/api", 784), head).is_empty());
        assert_eq!(missing(&roles, &[api, react, by_line], &pr_in("o/react", 784), head), ["cost review requested changes at head b4baa64 (a review of a newer head must pass)"]);
        assert_eq!((pr_in("o/api", 784).to_string(), pr(9).to_string()), ("o/api#784".to_string(), "#9".to_string()));
    }

    /// Review S1 on #201: a repo, in `pr:`, in a `repo:` line or from a
    /// gate's `--repo`, is read as lowercase `owner/repo` (or `repo`): a
    /// scheme, a host, `.git` and a PR URL's tail are dropped, and an old
    /// free-text `repo:` line that lists several names each.
    #[test]
    fn repos_are_read_as_lowercase_owner_repo() {
        let head = "b4baa646ffff";
        let parse = |meta: &str| Review::parse(&format!("role: cost\n{meta}\nhead: b4baa64\nverdict: pass")).unwrap_or_else(|| panic!("{meta}"));
        for p in ["o/api#784", "O/Api#784", "github.com/o/api#784", "https://github.com/o/api#784", "https://github.com/O/api.git#784", "https://github.com/o/api/pull/784", "git@github.com:o/api.git#784"] {
            assert_eq!(parse(&format!("pr: {p}")).prs, [pr_in("o/api", 784)], "{p}");
        }
        for line in ["https://github.com/o/api", "github.com/O/Api", "o/api/", "https://github.com/o/api/pull/784"] {
            assert_eq!(parse(&format!("pr: 784\nrepo: {line}")).prs, [pr_in("o/api", 784)], "{line}");
        }
        let two = parse("pr: 784\nrepo: api, react");
        assert!(two.reads(&pr_in("o/api", 784), head) && two.reads(&pr_in("o/react", 784), head) && !two.reads(&pr_in("o/web", 784), head));
        // A merge's `--repo github.com/o/api` names the PR filed as `o/api#784`.
        let api = parse("pr: o/api#784");
        assert!(api.reads(&Pr::parse("github.com/o/api#784", None).unwrap(), head));
        assert!(api.reads(&Pr::parse("784", Some("https://github.com/o/api")).unwrap(), head));
        assert!(!api.reads(&Pr::parse("784", Some("github.com/o/react")).unwrap(), head));
    }

    #[test]
    fn missing_names_each_role_and_request_changes_blocks_its_head() {
        let roles = vec!["simplicity".to_string(), "cost".to_string()];
        let head = "6e828cc0123456789";
        let r = |meta: &str| Review::parse(meta).unwrap();
        assert_eq!(missing(&roles, &[], &pr(99), head), ["simplicity review for head 6e828cc", "cost review for head 6e828cc"]);
        let old = r("review: simplicity, cost\npr: 99\nhead: d84126e\nverdict: request-changes (simplicity), pass (cost)");
        let cost = r("role: cost\npr: 99\nhead: 6e828cc\nverdict: pass");
        // A review of an older head does not count, either way.
        assert_eq!(missing(&roles, &[old.clone(), cost.clone()], &pr(99), head), ["simplicity review for head 6e828cc"]);
        let simp = r("role: simplicity\npr: 99\nhead: 6e828cc\nverdict: merge with follow-ups");
        assert!(missing(&roles, &[old, cost.clone(), simp.clone()], &pr(99), head).is_empty());
        // Changes requested on this head block even beside a pass of it.
        let no = r("role: simplicity\npr: 99\nhead: 6e828cc\nverdict: REQUEST CHANGES");
        assert_eq!(missing(&roles, &[simp, no, cost], &pr(99), head), ["simplicity review requested changes at head 6e828cc (a review of a newer head must pass)"]);
    }
}
