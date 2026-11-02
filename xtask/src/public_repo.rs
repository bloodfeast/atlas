//! The public-repository guard of `cargo xtask release`: nothing is pushed
//! (the release commit, its tag or the wiki) unless the public repository
//! is what its name says and only its owner can bypass its rules.
//!
//! GitHub keeps a redirect from a repository's old name to its new one, so
//! a name that once belonged to the development repository can still
//! resolve to it. Before a push the release asks GitHub (`gh api`) and
//! refuses unless:
//!
//! * every GitHub remote the release would push to names ATLAS_PUBLIC_REPO:
//!   `--remote` names `<repo>` and the wiki remote `<repo>.wiki`;
//! * `repos/<repo>` answers with `full_name` equal to ATLAS_PUBLIC_REPO
//!   (ASCII case ignored), so not a redirect, `private` false, and it is
//!   not the development repository (this checkout's `origin`);
//! * the owner is a `User`, not an `Organization`;
//! * every ruleset's `bypass_actors` is empty or exactly the repository
//!   admin role (`RepositoryRole`, actor id 5);
//! * `collaborators?affiliation=all` lists no one but the owner with admin
//!   permission;
//! * an active branch ruleset covers `main` (`~DEFAULT_BRANCH` or
//!   `refs/heads/main`) with the rules `deletion`, `non_fast_forward`,
//!   `required_linear_history` and `pull_request`, and an active tag
//!   ruleset covers `refs/tags/v*` with `deletion`, `update` and `creation`
//!   (see [`REQUIRED`]).
//!
//! [`linked_repos`] is the other half: the README, the wiki and the public
//! files must name the repository the release publishes to.
//!
//! `ATLAS_GITHUB_STUB` names a folder of saved answers in place of `gh api`
//! (each endpoint's path with every character outside `[A-Za-z0-9]` written
//! `_`, plus `.json`), so tests never touch the network. A local folder as
//! `--remote` pushes nowhere near GitHub; the release still runs the check
//! there and reports what a publish would do.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use atlas::json::Value;

type R<T = ()> = Result<T, String>;

/// The repository-role id GitHub gives the admin role in `bypass_actors`.
pub const ADMIN_ROLE_ID: i64 = 5;

/// A source of GitHub REST answers.
pub trait Api {
    /// The body of `GET <path>` (a path below the API root).
    fn get(&self, path: &str) -> R<String>;
}

/// `gh api`, as the user `gh` is logged in as.
pub struct Gh;

impl Api for Gh {
    fn get(&self, path: &str) -> R<String> {
        let o = Command::new("gh")
            .args(["api", path])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot ask GitHub for {path} (gh api: {e}); the release needs gh to check the public repository"))?;
        if !o.status.success() {
            return Err(format!("gh api {path}: {}", String::from_utf8_lossy(&o.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    }
}

/// Saved answers in a folder (see the module docs).
pub struct Stub(pub PathBuf);

/// The stub file name of an endpoint.
pub fn stub_name(path: &str) -> String {
    format!("{}.json", path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>())
}

impl Api for Stub {
    fn get(&self, path: &str) -> R<String> {
        let f = self.0.join(stub_name(path));
        std::fs::read_to_string(&f).map_err(|e| format!("gh api {path}: no stub answer ({}: {e})", f.display()))
    }
}

/// The API the release uses: the stub folder when ATLAS_GITHUB_STUB is set.
pub fn api() -> Box<dyn Api> {
    match std::env::var_os("ATLAS_GITHUB_STUB").filter(|v| !v.is_empty()) {
        Some(d) => Box::new(Stub(PathBuf::from(d))),
        None => Box::new(Gh),
    }
}

/// What GitHub says about a repository.
#[derive(Debug, PartialEq, Eq)]
pub struct RepoInfo {
    /// `owner/name` after any redirect.
    pub full_name: String,
    /// Whether the repository is private.
    pub private: bool,
    /// The owner's login.
    pub owner: String,
    /// `User` or `Organization`.
    pub owner_type: String,
}

fn json(path: &str, body: &str) -> R<Value> {
    atlas::json::parse(body).map_err(|e| format!("gh api {path}: not JSON ({e})"))
}

/// Reads the `repos/<owner>/<name>` answer.
pub fn parse_info(body: &str) -> R<RepoInfo> {
    let v = json("repos", body)?;
    let s = |v: Option<&Value>, what: &str| v.and_then(Value::as_str).map(str::to_string).ok_or(format!("the repository answer has no {what}"));
    Ok(RepoInfo {
        full_name: s(v.get("full_name"), "full_name")?,
        private: v.get("private").and_then(Value::as_bool).ok_or("the repository answer has no private flag")?,
        owner: s(v.get("owner").and_then(|o| o.get("login")), "owner.login")?,
        owner_type: s(v.get("owner").and_then(|o| o.get("type")), "owner.type")?,
    })
}

/// `owner/name` of a GitHub remote URL (https, `git@github.com:` or
/// `ssh://git@github.com/`), without `.git`; `None` for anything else.
pub fn github_slug(url: &str) -> Option<String> {
    let rest = ["https://github.com/", "http://github.com/", "git@github.com:", "ssh://git@github.com/", "git://github.com/"]
        .iter()
        .find_map(|p| url.strip_prefix(p))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (parts.next().is_none() && !owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

/// The verdict on the repository answer: `want` is ATLAS_PUBLIC_REPO, `dev`
/// the development repository's `owner/name` when known.
pub fn verdict(want: &str, info: &RepoInfo, dev: Option<&str>) -> R {
    let vis = if info.private { "private" } else { "public" };
    if !info.full_name.eq_ignore_ascii_case(want) {
        return Err(format!("{want} resolves to {} ({vis}): create the public repo first", info.full_name));
    }
    if dev.is_some_and(|d| d.eq_ignore_ascii_case(&info.full_name)) {
        return Err(format!("{want} is the development repository ({vis}): set ATLAS_PUBLIC_REPO to the public one"));
    }
    if info.private {
        return Err(format!("{want} is private: the release publishes only to a public repository"));
    }
    if info.owner_type != "User" {
        return Err(format!("{want} is owned by the {} {}: the public repository must belong to a user account", info.owner_type, info.owner));
    }
    Ok(())
}

/// Every GitHub remote must name `want` (the wiki remote `want.wiki`).
pub fn check_remotes(want: &str, remotes: &[&str]) -> R {
    for r in remotes {
        if let Some(slug) = github_slug(r) {
            let base = slug.strip_suffix(".wiki").unwrap_or(&slug);
            if !base.eq_ignore_ascii_case(want) {
                return Err(format!("the remote {r} is not {want} (ATLAS_PUBLIC_REPO)"));
            }
        }
    }
    Ok(())
}

/// A ruleset's `bypass_actors` must be empty or exactly the admin role.
pub fn check_bypass(want: &str, ruleset: &Value) -> R {
    let name = ruleset.get("name").and_then(Value::as_str).unwrap_or("?");
    let actors = ruleset.get("bypass_actors").and_then(Value::as_array).ok_or(format!("{want}: ruleset {name:?} lists no bypass_actors (is gh logged in as the owner?)"))?;
    let admin_only = |a: &Value| a.get("actor_type").and_then(Value::as_str) == Some("RepositoryRole") && a.get("actor_id").and_then(Value::as_i64) == Some(ADMIN_ROLE_ID);
    match actors.as_slice() {
        [] => Ok(()),
        [a] if admin_only(a) => Ok(()),
        _ => {
            let who: Vec<String> = actors
                .iter()
                .map(|a| {
                    let t = a.get("actor_type").and_then(Value::as_str).unwrap_or("?");
                    let id = a.get("actor_id").map(|i| i.as_i64().map_or("null".to_string(), |i| i.to_string())).unwrap_or("null".into());
                    format!("{t} {id}")
                })
                .collect();
            Err(format!("{want}: ruleset {name:?} lets {} bypass it; only the admin role (RepositoryRole {ADMIN_ROLE_ID}) may", who.join(", ")))
        }
    }
}

/// No collaborator but the owner may hold admin permission.
pub fn check_collaborators(want: &str, owner: &str, list: &Value) -> R {
    let list = list.as_array().ok_or(format!("{want}: the collaborators answer is not a list"))?;
    let admins: Vec<&str> = list
        .iter()
        .filter(|c| c.get("permissions").and_then(|p| p.get("admin")).and_then(Value::as_bool) == Some(true) || c.get("role_name").and_then(Value::as_str) == Some("admin"))
        .filter_map(|c| c.get("login").and_then(Value::as_str))
        .filter(|l| !l.eq_ignore_ascii_case(owner))
        .collect();
    if admins.is_empty() {
        Ok(())
    } else {
        Err(format!("{want}: {} {} admin besides the owner {owner}; only the owner may bypass a ruleset", admins.join(", "), if admins.len() == 1 { "is" } else { "are" }))
    }
}

/// The rulesets the public repository must have: `(what, target, refs one
/// of which the ruleset must include, rule types it must hold)`.
pub const REQUIRED: &[(&str, &str, &[&str], &[&str])] = &[
    ("a main branch ruleset", "branch", &["~DEFAULT_BRANCH", "refs/heads/main"], &["deletion", "non_fast_forward", "required_linear_history", "pull_request"]),
    ("a v* tag ruleset", "tag", &["refs/tags/v*"], &["deletion", "update", "creation"]),
];

/// Every [`REQUIRED`] ruleset is among `rulesets` (their full answers),
/// active, on the right refs and with every rule.
pub fn check_required(want: &str, rulesets: &[Value]) -> R {
    let strs = |v: Option<&Value>| -> Vec<String> { v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default() };
    let mut missing = Vec::new();
    for (what, target, refs, rules) in REQUIRED {
        let ok = rulesets.iter().any(|r| {
            let types: Vec<String> = r.get("rules").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.get("type").and_then(Value::as_str)).map(str::to_string).collect()).unwrap_or_default();
            let include = strs(r.get("conditions").and_then(|c| c.get("ref_name")).and_then(|n| n.get("include")));
            r.get("target").and_then(Value::as_str) == Some(*target)
                && r.get("enforcement").and_then(Value::as_str) == Some("active")
                && refs.iter().any(|x| include.iter().any(|i| i == x))
                && rules.iter().all(|x| types.iter().any(|t| t == x))
        });
        if !ok {
            missing.push(format!("{what} (active, on {}, with {})", refs.join(" or "), rules.join(", ")));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{want} lacks {}: set up the rulesets the Releasing page lists first", missing.join(" and ")))
    }
}

/// The public repositories the docs and public files name: wiki and
/// security links (`github.com/OWNER/NAME/wiki`, `/security`), the
/// marketplace command, and the CI gate (`github.repository == '...'`).
/// Placeholders such as `<public repo>` are not names.
pub fn linked_repos(text: &str) -> Vec<String> {
    let slug = |s: &str| -> Option<String> {
        let mut parts = s.splitn(3, '/');
        let (o, n) = (parts.next()?, parts.next()?);
        let ok = |x: &str| !x.is_empty() && x.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        (ok(o) && ok(n)).then(|| format!("{o}/{n}"))
    };
    let mut out = Vec::new();
    for (i, _) in text.match_indices("github.com/") {
        let rest = &text[i + "github.com/".len()..];
        let end = rest.find(|c: char| c.is_whitespace() || matches!(c, ')' | '>' | '`' | '"' | '\'')).unwrap_or(rest.len());
        let url = &rest[..end];
        let mut parts = url.splitn(3, '/');
        let (Some(o), Some(n), Some(tail)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if tail.starts_with("wiki") || tail.starts_with("security") {
            out.extend(slug(&format!("{o}/{n}")));
        }
    }
    for marker in ["/plugin marketplace add ", "github.repository == '"] {
        for (i, _) in text.match_indices(marker) {
            let rest = &text[i + marker.len()..];
            let end = rest.find(|c: char| c.is_whitespace() || c == '\'').unwrap_or(rest.len());
            out.extend(slug(&rest[..end]));
        }
    }
    out
}

/// Files under `root` that name a public repository other than `want`
/// (README.md, SECURITY.md, the CI and release workflows and every wiki
/// page).
pub fn link_mismatches(root: &std::path::Path, want: &str) -> R<Vec<String>> {
    let mut files: Vec<std::path::PathBuf> = ["README.md", "SECURITY.md", ".github/workflows/ci.yml", ".github/workflows/release.yml"].iter().map(|f| root.join(f)).collect();
    if let Ok(rd) = std::fs::read_dir(root.join("wiki")) {
        let mut w: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
        w.sort();
        files.extend(w);
    }
    let mut out = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let mut repos = linked_repos(&text);
        repos.sort();
        repos.dedup();
        for r in repos {
            if !r.eq_ignore_ascii_case(want) {
                out.push(format!("{} names {r}", f.strip_prefix(root).unwrap_or(&f).display()));
            }
        }
    }
    Ok(out)
}

/// What the guard found, when it passes.
#[derive(Debug)]
pub struct Report {
    /// Rulesets checked.
    pub rulesets: usize,
    /// Collaborators listed.
    pub collaborators: usize,
    /// What a token without the owner's view could not see (always empty
    /// for [`check`]); the owner's full check covers it later.
    pub unseen: Vec<String>,
}

/// The whole guard (see the module docs). `dev_remote` is this checkout's
/// `origin` URL.
pub fn check(api: &dyn Api, want: &str, dev_remote: Option<&str>, remotes: &[&str]) -> R<Report> {
    check_with(api, want, dev_remote, remotes, true)
}

/// The guard as a token without admin rights sees it (the release app's
/// installation token in `cargo xtask release --pr`): GitHub leaves
/// `bypass_actors` out of a ruleset answer for such a token, and may refuse
/// it the collaborator list. Those two parts are reported in
/// [`Report::unseen`] instead of refused; everything else is checked as in
/// [`check`], and a `bypass_actors` that is present is still checked. The
/// owner's `cargo xtask release sign` makes the full check before anything
/// is published.
pub fn check_unprivileged(api: &dyn Api, want: &str, dev_remote: Option<&str>, remotes: &[&str]) -> R<Report> {
    check_with(api, want, dev_remote, remotes, false)
}

fn check_with(api: &dyn Api, want: &str, dev_remote: Option<&str>, remotes: &[&str], owner: bool) -> R<Report> {
    check_remotes(want, remotes)?;
    let path = format!("repos/{want}");
    let info = parse_info(&api.get(&path).map_err(|e| format!("{want} was not found on GitHub ({e}): create the public repo first"))?)?;
    let dev = dev_remote.and_then(github_slug);
    verdict(want, &info, dev.as_deref())?;
    let lp = format!("repos/{want}/rulesets?per_page=100");
    let list = json(&lp, &api.get(&lp)?)?;
    let list = list.as_array().ok_or(format!("gh api {lp}: not a list"))?;
    let mut full = Vec::new();
    let mut unseen = Vec::new();
    for r in list {
        let id = r.get("id").and_then(Value::as_i64).ok_or(format!("gh api {lp}: a ruleset without an id"))?;
        let rp = format!("repos/{want}/rulesets/{id}");
        let v = json(&rp, &api.get(&rp)?)?;
        if owner || v.get("bypass_actors").is_some() {
            check_bypass(want, &v)?;
        } else {
            unseen.push(format!("the bypass list of ruleset {:?}", v.get("name").and_then(Value::as_str).unwrap_or("?")));
        }
        full.push(v);
    }
    check_required(want, &full)?;
    let cp = format!("repos/{want}/collaborators?affiliation=all&per_page=100");
    let collab = match api.get(&cp) {
        Ok(body) => Some(json(&cp, &body)?),
        Err(_) if !owner => {
            unseen.push("the collaborator list".into());
            None
        }
        Err(e) => return Err(e),
    };
    if let Some(c) = &collab {
        check_collaborators(want, &info.owner, c)?;
    }
    let collaborators = collab.as_ref().and_then(Value::as_array).map_or(0, Vec::len);
    Ok(Report { rulesets: list.len(), collaborators, unseen })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Map(HashMap<String, String>);

    impl Api for Map {
        fn get(&self, path: &str) -> R<String> {
            self.0.get(path).cloned().ok_or(format!("HTTP 404 for {path}"))
        }
    }

    const REPO: &str = r#"{"full_name": "acme/tool", "private": false, "owner": {"login": "acme", "type": "User"}}"#;
    const MAIN_RULES: &str = r#""target": "branch", "enforcement": "active", "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH", "refs/heads/main"], "exclude": []}}, "rules": [{"type": "deletion"}, {"type": "non_fast_forward"}, {"type": "required_linear_history"}, {"type": "pull_request", "parameters": {"required_approving_review_count": 1}}]"#;
    const TAG_RULES: &str = r#""target": "tag", "enforcement": "active", "conditions": {"ref_name": {"include": ["refs/tags/v*"], "exclude": []}}, "rules": [{"type": "deletion"}, {"type": "update"}, {"type": "creation"}]"#;
    const ADMIN_ONLY: &str = r#"[{"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "always"}]"#;

    /// A public repository set up as the Releasing page says.
    fn good() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("repos/acme/tool".into(), REPO.into());
        m.insert("repos/acme/tool/rulesets?per_page=100".into(), r#"[{"id": 11, "name": "main"}, {"id": 12, "name": "tags"}]"#.into());
        m.insert("repos/acme/tool/rulesets/11".into(), format!(r#"{{"id": 11, "name": "main", "bypass_actors": {ADMIN_ONLY}, {MAIN_RULES}}}"#));
        m.insert("repos/acme/tool/rulesets/12".into(), format!(r#"{{"id": 12, "name": "tags", "bypass_actors": [], {TAG_RULES}}}"#));
        m.insert(
            "repos/acme/tool/collaborators?affiliation=all&per_page=100".into(),
            r#"[{"login": "acme", "permissions": {"admin": true}}, {"login": "helper", "permissions": {"admin": false, "push": true}, "role_name": "write"}]"#.into(),
        );
        m
    }

    fn run(m: HashMap<String, String>) -> R<Report> {
        check(&Map(m), "acme/tool", Some("https://github.com/acme/tool-dev.git"), &["https://github.com/acme/tool.git", "https://github.com/acme/tool.wiki.git"])
    }

    #[test]
    fn a_well_set_up_public_repository_passes() {
        let r = run(good()).unwrap();
        assert_eq!((r.rulesets, r.collaborators), (2, 2));
    }

    #[test]
    fn a_redirect_to_the_private_dev_repo_is_refused_by_name() {
        let mut m = good();
        m.insert("repos/acme/tool".into(), r#"{"id": 1, "full_name": "acme/tool-dev", "private": true, "owner": {"login": "acme", "type": "User"}}"#.into());
        assert_eq!(run(m).unwrap_err(), "acme/tool resolves to acme/tool-dev (private): create the public repo first");
    }

    #[test]
    fn missing_private_dev_and_org_owned_repositories_are_refused() {
        let mut m = good();
        m.remove("repos/acme/tool");
        assert!(run(m).unwrap_err().contains("not found on GitHub"));
        let info = |full: &str, private: bool, ty: &str| RepoInfo { full_name: full.into(), private, owner: "acme".into(), owner_type: ty.into() };
        assert!(verdict("Acme/Tool", &info("acme/tool", false, "User"), Some("acme/tool-dev")).is_ok());
        assert!(verdict("acme/tool", &info("acme/tool", true, "User"), None).unwrap_err().contains("is private"));
        assert!(verdict("acme/tool", &info("acme/tool", false, "User"), Some("ACME/tool")).unwrap_err().contains("development repository"));
        assert!(verdict("acme/tool", &info("acme/tool", false, "Organization"), None).unwrap_err().contains("Organization acme"));
        assert!(parse_info(r#"{"full_name": "a/b", "private": false}"#).is_err());
        assert!(parse_info("not json").is_err());
    }

    #[test]
    fn a_bypass_actor_other_than_the_admin_role_is_refused() {
        for actors in [
            r#"[{"actor_id": 5, "actor_type": "RepositoryRole"}, {"actor_id": 42, "actor_type": "Integration"}]"#,
            r#"[{"actor_id": 4, "actor_type": "RepositoryRole"}]"#,
            r#"[{"actor_id": null, "actor_type": "OrganizationAdmin"}]"#,
            r#"[{"actor_id": 7, "actor_type": "Team"}]"#,
        ] {
            let mut m = good();
            m.insert("repos/acme/tool/rulesets/12".into(), format!(r#"{{"id": 12, "name": "tags", "bypass_actors": {actors}, {TAG_RULES}}}"#));
            let e = run(m).unwrap_err();
            assert!(e.contains("ruleset \"tags\" lets") && e.contains("only the admin role"), "{e}");
        }
        let mut m = good();
        m.insert("repos/acme/tool/rulesets/11".into(), r#"{"id": 11, "name": "main"}"#.into());
        assert!(run(m).unwrap_err().contains("lists no bypass_actors"));
    }

    #[test]
    fn the_main_and_tag_rulesets_must_exist_with_their_rules() {
        // No rulesets at all.
        let mut m = good();
        m.insert("repos/acme/tool/rulesets?per_page=100".into(), "[]".into());
        let e = run(m).unwrap_err();
        assert!(e.contains("lacks a main branch ruleset") && e.contains("and a v* tag ruleset"), "{e}");
        // The main ruleset without linear history.
        let mut m = good();
        let weak = MAIN_RULES.replace(r#"{"type": "required_linear_history"}, "#, "");
        m.insert("repos/acme/tool/rulesets/11".into(), format!(r#"{{"id": 11, "name": "main", "bypass_actors": [], {weak}}}"#));
        let e = run(m).unwrap_err();
        assert!(e.contains("lacks a main branch ruleset") && !e.contains("tag ruleset"), "{e}");
        // The tag ruleset disabled, or on the wrong refs.
        for (from, to) in [(r#""active""#, r#""disabled""#), ("refs/tags/v*", "refs/tags/*")] {
            let mut m = good();
            let t = TAG_RULES.replace(from, to);
            m.insert("repos/acme/tool/rulesets/12".into(), format!(r#"{{"id": 12, "name": "tags", "bypass_actors": [], {t}}}"#));
            let e = run(m).unwrap_err();
            assert!(e.contains("lacks a v* tag ruleset"), "{e}");
        }
    }

    #[test]
    fn docs_must_name_the_repository_the_release_publishes_to() {
        let t = "see https://github.com/acme/tool/wiki/Home and [x](https://github.com/acme/tool/security/advisories/new)\n\
                 /plugin marketplace add acme/tool\nif: github.repository == 'acme/tool'\n\
                 open `https://github.com/<public repo>/wiki` and https://github.com/other/lib (not a wiki link)";
        assert_eq!(linked_repos(t), ["acme/tool", "acme/tool", "acme/tool", "acme/tool"]);
        let d = std::env::temp_dir().join(format!("atlas-linked-{}", std::process::id()));
        std::fs::create_dir_all(d.join("wiki")).unwrap();
        std::fs::write(d.join("README.md"), "[w](https://github.com/acme/tool/wiki/Home)\n").unwrap();
        std::fs::write(d.join("wiki/Home.md"), "[[Overview]] https://github.com/acme/tool-dev/wiki\n").unwrap();
        let got = link_mismatches(&d, "acme/tool").unwrap();
        let ok = link_mismatches(&d, "acme/tool-dev").unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(got.len(), 1);
        assert!(got[0].ends_with("Home.md names acme/tool-dev"), "{got:?}");
        assert_eq!(ok, ["README.md names acme/tool"]);
    }

    #[test]
    fn the_shipped_docs_name_the_default_public_repo() {
        let root = crate::release::repo_root();
        assert_eq!(link_mismatches(&root, crate::release::DEFAULT_PUBLIC_REPO).unwrap(), Vec::<String>::new());
        assert!(linked_repos(&std::fs::read_to_string(root.join("README.md")).unwrap()).len() > 5);
    }

    #[test]
    fn an_unprivileged_token_reports_what_it_cannot_see() {
        // The app's token: no bypass lists and no collaborator list.
        let mut m = good();
        m.insert("repos/acme/tool/rulesets/11".into(), format!(r#"{{"id": 11, "name": "main", {MAIN_RULES}}}"#));
        m.remove("repos/acme/tool/collaborators?affiliation=all&per_page=100");
        assert!(run(m.clone()).unwrap_err().contains("lists no bypass_actors"), "the owner's check still refuses");
        let r = check_unprivileged(&Map(m.clone()), "acme/tool", None, &[]).unwrap();
        assert_eq!(r.unseen, [r#"the bypass list of ruleset "main""#, "the collaborator list"]);
        // A bypass list it can see is still checked, and so is everything else.
        let mut bad = m.clone();
        bad.insert(
            "repos/acme/tool/rulesets/12".into(),
            format!(r#"{{"id": 12, "name": "tags", "bypass_actors": [{{"actor_id": 9, "actor_type": "Integration"}}], {TAG_RULES}}}"#),
        );
        assert!(check_unprivileged(&Map(bad), "acme/tool", None, &[]).unwrap_err().contains("Integration 9"));
        m.insert("repos/acme/tool".into(), r#"{"full_name": "acme/tool", "private": true, "owner": {"login": "acme", "type": "User"}}"#.into());
        assert!(check_unprivileged(&Map(m), "acme/tool", None, &[]).unwrap_err().contains("is private"));
    }

    #[test]
    fn a_second_admin_is_refused() {
        let mut m = good();
        m.insert(
            "repos/acme/tool/collaborators?affiliation=all&per_page=100".into(),
            r#"[{"login": "acme", "permissions": {"admin": true}}, {"login": "mallory", "permissions": {"admin": true}}]"#.into(),
        );
        assert_eq!(run(m).unwrap_err(), "acme/tool: mallory is admin besides the owner acme; only the owner may bypass a ruleset");
    }

    #[test]
    fn remote_slugs_and_the_wiki_remote() {
        assert_eq!(github_slug("https://github.com/acme/tool.git").as_deref(), Some("acme/tool"));
        assert_eq!(github_slug("git@github.com:acme/tool.wiki.git").as_deref(), Some("acme/tool.wiki"));
        assert_eq!(github_slug("ssh://git@github.com/acme/tool/").as_deref(), Some("acme/tool"));
        assert_eq!(github_slug("Z:/stage/pub.git"), None);
        assert!(check_remotes("acme/tool", &["https://github.com/acme/tool.git", "https://github.com/acme/tool.wiki.git", "/tmp/x.git"]).is_ok());
        let e = check_remotes("acme/tool", &["git@github.com:acme/tool-dev.git"]).unwrap_err();
        assert!(e.contains("is not acme/tool"), "{e}");
    }

    #[test]
    fn the_stub_folder_names_endpoints_plainly() {
        assert_eq!(stub_name("repos/acme/tool/collaborators?affiliation=all&per_page=100"), "repos_acme_tool_collaborators_affiliation_all_per_page_100.json");
        let d = std::env::temp_dir().join(format!("atlas-gh-stub-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for (k, v) in good() {
            std::fs::write(d.join(stub_name(&k)), v).unwrap();
        }
        let r = check(&Stub(d.clone()), "acme/tool", None, &[]);
        let _ = std::fs::remove_dir_all(&d);
        assert!(r.is_ok(), "{r:?}");
    }
}
