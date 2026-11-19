//! Direct reads of Jira and Confluence over their REST APIs, with a token
//! atlas never holds (journal:ATLAS-TRACKERS:0001, 0002).
//!
//! The configuration names a site and the environment variables that hold
//! the credential, never the credential itself:
//!
//! ```json
//! {"project_tracking": {"atlassian": {
//!   "site": "https://example.atlassian.net",
//!   "email_env": "ATLASSIAN_EMAIL", "token_env": "ATLASSIAN_API_TOKEN"}}}
//! ```
//!
//! Cloud uses basic auth (an account email and an API token); Data Center
//! and Server use a personal access token as a bearer (`pat_env`). The
//! variables are read when a request is made, kept in memory for that call
//! only ([`Secret`] wipes its bytes on drop and prints as `[redacted]`), and
//! never written to the store, the configuration, logs, handles, errors,
//! hook output or tool answers: every message a failure can produce is
//! passed through [`Credential::scrub`]. A secret manager (an AWS ARN,
//! `op read`, a vault) is the harness's job: it resolves the secret into the
//! named variable before atlas starts (journal:ATLAS-TRACKERS:0002).
//!
//! The client is read-only by construction: [`http::Method`] has one
//! variant, `GET`, and every path it may send is in [`api::ENDPOINTS`].
//! Writes stay on the Atlassian MCP route, behind Claude Code's permission
//! prompts.
//!
//! ```text
//! atlas track check                 config, variables present or absent, one whoami, write capability
//! atlas track smoke [--issue KEY] [--page ID] [--jql Q] [--cql Q]   shapes and sizes only
//! atlas track issue KEY | jql Q [--limit N] | page ID | cql Q [--limit N]   a digest; the raw response kept as a handle
//! atlas track snapshot ITEM [--status S --assignee A --summary T]   a queue item's ticket snapshot
//! ```

pub mod api;
pub mod digest;
pub mod http;
pub mod plan;
pub mod snapshot;

#[cfg(test)]
mod fix_tests;
#[cfg(test)]
mod queue_tests;
#[cfg(test)]
mod reads_tests;

use std::fmt;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use crate::json::Value;

/// The longest environment variable name accepted.
pub const MAX_ENV_NAME: usize = 64;

/// How the credential is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRef {
    /// Atlassian Cloud: basic auth from an account email and an API token.
    Cloud {
        /// The variable holding the account email.
        email_env: String,
        /// The variable holding the API token.
        token_env: String,
    },
    /// Data Center or Server: a personal access token sent as a bearer.
    Pat {
        /// The variable holding the token.
        pat_env: String,
    },
}

impl AuthRef {
    /// The variable names, in the order `check` reports them.
    pub fn env_names(&self) -> Vec<&str> {
        match self {
            AuthRef::Cloud { email_env, token_env } => vec![email_env, token_env],
            AuthRef::Pat { pat_env } => vec![pat_env],
        }
    }

    /// True for Atlassian Cloud.
    pub fn is_cloud(&self) -> bool {
        matches!(self, AuthRef::Cloud { .. })
    }
}

/// `project_tracking.atlassian`, read and checked. It holds names, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The site (`https://example.atlassian.net`), no trailing slash.
    pub site: String,
    /// Confluence's own site on Data Center, when it is not `site`.
    pub confluence_site: Option<String>,
    /// The credential's variables.
    pub auth: AuthRef,
    /// Confluence's own credential on Cloud (`confluence_token_env`, with
    /// `confluence_email_env` or else `email_env`), when it is not `auth`:
    /// a scoped token holds one product's scopes only (issue #304).
    pub confluence_auth: Option<AuthRef>,
    /// Seconds one request may take.
    pub timeout_s: u64,
    /// The least time between two requests to one site, in milliseconds.
    pub min_interval_ms: u64,
    /// Retries after a 429 or a 5xx.
    pub max_retries: u32,
    /// The longest wait for one retry, in seconds (a longer `Retry-After`
    /// is capped to it).
    pub max_wait_s: u64,
    /// The first backoff when no `Retry-After` is given, doubled per try.
    pub backoff_ms: u64,
}

/// True for an environment variable name atlas accepts: upper case
/// letters, digits and `_`, not starting with a digit, at most
/// [`MAX_ENV_NAME`] characters. A token pasted where a name belongs fails
/// this (tokens hold lower case letters and punctuation).
pub fn env_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_ENV_NAME
        && !n.as_bytes()[0].is_ascii_digit()
        && n.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// True for a site URL: `https` with a host and nothing after it but an
/// optional context path, or plain `http` to loopback (the tests' server).
/// No user information, query or fragment.
pub fn site_ok(s: &str) -> bool {
    if s.contains(['@', '?', '#', ' ', '"', '\\']) || s.chars().any(char::is_control) {
        return false;
    }
    let host = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).map(|r| r.split('/').next().unwrap_or(""));
    host.is_some_and(|h| !h.is_empty()) && crate::models::url_allowed(&format!("{}/", s.trim_end_matches('/')))
}

impl Settings {
    /// Reads the `project_tracking` section; `None` when it is absent or
    /// names no `atlassian` site.
    ///
    /// # Errors
    ///
    /// A message naming the field. A field that should name a variable but
    /// does not is refused without its value in the message, since it may be
    /// a pasted token.
    pub fn from_config(v: Option<&Value>) -> std::result::Result<Option<Settings>, String> {
        let Some(v) = v else { return Ok(None) };
        let top = crate::config::Fields::new("project_tracking", v, &["atlassian"]).map_err(|e| e.to_string())?;
        let Some(a) = top.get("atlassian") else { return Ok(None) };
        let f = crate::config::Fields::new(
            "project_tracking.atlassian",
            a,
            &[
                "site",
                "confluence_site",
                "email_env",
                "token_env",
                "pat_env",
                "confluence_email_env",
                "confluence_token_env",
                "timeout_s",
                "min_interval_ms",
                "max_retries",
                "max_wait_s",
                "backoff_ms",
            ],
        )
        .map_err(|e| e.to_string())?;
        let env = |k: &str| -> std::result::Result<Option<String>, String> {
            match f.get(k) {
                None => Ok(None),
                Some(Value::String(s)) if env_name_ok(s) => Ok(Some(s.clone())),
                Some(_) => Err(format!(
                    "project_tracking.atlassian.{k} must name an environment variable (A-Z, 0-9 and _, at most {MAX_ENV_NAME} characters), not hold a credential; the value given is not shown"
                )),
            }
        };
        let (email, token, pat) = (env("email_env")?, env("token_env")?, env("pat_env")?);
        let auth = match (email, token, pat) {
            (Some(email_env), Some(token_env), None) => AuthRef::Cloud { email_env, token_env },
            (None, None, Some(pat_env)) => AuthRef::Pat { pat_env },
            (_, _, Some(_)) => {
                return Err("project_tracking.atlassian: give pat_env (Data Center) or email_env and token_env (Cloud), not both".into())
            }
            _ => return Err("project_tracking.atlassian needs email_env and token_env (Cloud) or pat_env (Data Center)".into()),
        };
        let confluence_auth = match (&auth, env("confluence_email_env")?, env("confluence_token_env")?) {
            (_, None, None) => None,
            (AuthRef::Cloud { email_env, .. }, ce, Some(token_env)) => {
                Some(AuthRef::Cloud { email_env: ce.unwrap_or_else(|| email_env.clone()), token_env })
            }
            (AuthRef::Cloud { .. }, Some(_), None) => {
                return Err("project_tracking.atlassian.confluence_email_env needs confluence_token_env beside it".into())
            }
            (AuthRef::Pat { .. }, _, _) => {
                return Err(
                    "project_tracking.atlassian: confluence_token_env and confluence_email_env are for Cloud, not with pat_env".into()
                )
            }
        };
        let site_of = |k: &str| -> std::result::Result<Option<String>, String> {
            match f.string(k).map_err(|e| e.to_string())? {
                None => Ok(None),
                Some(s) if site_ok(&s) => Ok(Some(s.trim_end_matches('/').to_string())),
                Some(_) => Err(format!(
                    "project_tracking.atlassian.{k} must be an https URL with no user, query or fragment (plain http only to loopback)"
                )),
            }
        };
        let site = site_of("site")?.ok_or("project_tracking.atlassian.site is required")?;
        let confluence_site = site_of("confluence_site")?;
        let num =
            |k: &str, d: u64| -> std::result::Result<u64, String> { Ok(f.uint(k).map_err(|e| e.to_string())?.map_or(d, |n| n as u64)) };
        Ok(Some(Settings {
            site,
            confluence_site,
            auth,
            confluence_auth,
            timeout_s: num("timeout_s", 30)?.clamp(1, 600),
            min_interval_ms: num("min_interval_ms", 200)?.min(60_000),
            max_retries: u32::try_from(num("max_retries", 3)?.min(10)).unwrap_or(3),
            max_wait_s: num("max_wait_s", 60)?.clamp(1, 600),
            backoff_ms: num("backoff_ms", 500)?.clamp(1, 60_000),
        }))
    }

    /// Jira's base URL.
    pub fn jira_base(&self) -> &str {
        &self.site
    }

    /// Confluence's base URL: `confluence_site` when given, else
    /// `<site>/wiki` on Cloud or the site on Data Center.
    ///
    /// # Errors
    ///
    /// [`CONFLUENCE_SITE_NEEDED`] when `site` is Jira's gateway and
    /// `confluence_site` is unset: `<gateway>/wiki` is Jira's gateway, not
    /// Confluence's, so it is never derived.
    pub fn confluence_base(&self) -> std::result::Result<String, String> {
        match (&self.auth, &self.confluence_site) {
            (_, Some(c)) => Ok(c.clone()),
            _ if self.site_is_jira_gateway() => Err(CONFLUENCE_SITE_NEEDED.to_string()),
            (AuthRef::Cloud { .. }, None) => Ok(format!("{}/wiki", self.site)),
            (AuthRef::Pat { .. }, None) => Ok(self.site.clone()),
        }
    }

    /// True when `site` is Jira's gateway (`https://api.atlassian.com/ex/jira/<cloud id>`).
    pub fn site_is_jira_gateway(&self) -> bool {
        let rest = self.site.split("://").nth(1).unwrap_or("");
        let path = rest.find('/').map_or("", |i| &rest[i..]);
        path.to_ascii_lowercase().starts_with("/ex/jira/")
    }

    /// Whether Jira's variables are set and not empty (the values are read
    /// and dropped at once). Confluence's own variable is not needed for a
    /// Jira read: it is read on the first Confluence request.
    pub fn env_present(&self, env: &dyn Fn(&str) -> Option<String>) -> bool {
        self.auth.env_names().iter().all(|n| env(n).is_some_and(|v| !v.is_empty()))
    }

    /// Every variable named, Jira's first, each once.
    pub fn env_names(&self) -> Vec<&str> {
        let mut v = self.auth.env_names();
        for n in self.confluence_auth.iter().flat_map(AuthRef::env_names) {
            if !v.contains(&n) {
                v.push(n);
            }
        }
        v
    }

    /// The credential Confluence reads use: its own when configured, else
    /// Jira's.
    pub fn confluence_auth(&self) -> &AuthRef {
        self.confluence_auth.as_ref().unwrap_or(&self.auth)
    }

    /// The per-request timeout.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_s)
    }

    /// The site's host class for output that may be pasted elsewhere: the
    /// host itself is a company's name, so it is never printed.
    pub fn host_class(&self) -> &'static str {
        HostClass::of(&self.site).describe()
    }

    /// True when `site` is an Atlassian Cloud site URL (`*.atlassian.net`).
    pub fn is_cloud_site(&self) -> bool {
        HostClass::of(&self.site) == HostClass::Cloud
    }
}

/// What kind of host a site URL names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    /// An Atlassian Cloud site URL, `*.atlassian.net`.
    Cloud,
    /// Loopback (the tests' server).
    Loopback,
    /// Anything else: Data Center, or Atlassian's gateway.
    Custom,
}

impl HostClass {
    /// The class of `url`'s host.
    pub fn of(url: &str) -> HostClass {
        let host = url.split("://").nth(1).unwrap_or("").split(['/', ':']).next().unwrap_or("").to_ascii_lowercase();
        if host.ends_with(".atlassian.net") {
            HostClass::Cloud
        } else if matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
            HostClass::Loopback
        } else {
            HostClass::Custom
        }
    }

    /// The class in words, for `check`.
    pub fn describe(self) -> &'static str {
        match self {
            HostClass::Cloud => "an Atlassian Cloud host (*.atlassian.net)",
            HostClass::Loopback => "a loopback host",
            HostClass::Custom => "a custom host",
        }
    }
}

/// Why a Confluence read is refused when `site` is Jira's gateway and
/// `confluence_site` is unset (issue #304).
pub const CONFLUENCE_SITE_NEEDED: &str = "site is Jira's gateway (https://api.atlassian.com/ex/jira/<cloud id>), so Confluence needs its own: set confluence_site to https://api.atlassian.com/ex/confluence/<cloud id>/wiki, and confluence_token_env to the variable holding a token with Confluence's scopes";

/// A credential value: kept in memory only, printed as `[redacted]`, and
/// overwritten when dropped.
pub struct Secret(String);

impl Secret {
    /// Wraps a value.
    pub fn new(s: String) -> Secret {
        Secret(s)
    }

    /// The value, for the one place it is sent.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        let mut b = std::mem::take(&mut self.0).into_bytes();
        b.iter_mut().for_each(|x| *x = 0);
        std::hint::black_box(&b);
    }
}

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Every base64 spelling of `data` an echo could carry, longest first:
/// standard with and without padding, and URL-safe with and without
/// padding, each once.
fn base64_forms(data: &[u8]) -> Vec<Secret> {
    let std = Secret::new(base64(data));
    let url = Secret::new(
        std.expose()
            .chars()
            .map(|c| match c {
                '+' => '-',
                '/' => '_',
                c => c,
            })
            .collect(),
    );
    let mut out: Vec<Secret> = Vec::with_capacity(4);
    for f in [std.expose(), std.expose().trim_end_matches('='), url.expose(), url.expose().trim_end_matches('=')] {
        if !out.iter().any(|o| o.expose() == f) {
            out.push(Secret::new(f.to_string()));
        }
    }
    out
}

/// The credential for one command: the `Authorization` header value and
/// every form of the secret a message could carry, for [`Credential::scrub`].
pub struct Credential {
    header: Secret,
    forms: Vec<Secret>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential([redacted])")
    }
}

/// Why the credential could not be read: names the variable, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing(pub String);

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the environment variable {} is not set (or empty)", self.0)
    }
}

impl Credential {
    /// Reads the variables now. Called once per command, never cached
    /// across commands.
    ///
    /// # Errors
    ///
    /// [`Missing`] naming the first variable that is unset or empty.
    pub fn resolve(auth: &AuthRef, env: &dyn Fn(&str) -> Option<String>) -> std::result::Result<Credential, Missing> {
        let get = |n: &str| env(n).filter(|v| !v.is_empty()).map(Secret::new).ok_or_else(|| Missing(n.to_string()));
        match auth {
            AuthRef::Cloud { email_env, token_env } => {
                let email = get(email_env)?;
                let token = get(token_env)?;
                let pair = Secret::new(format!("{}:{}", email.expose(), token.expose()));
                let b64 = Secret::new(base64(pair.expose().as_bytes()));
                let header = Secret::new(format!("Basic {}", b64.expose()));
                let tok64 = base64_forms(token.expose().as_bytes());
                // The auth material only: the account email alone is not a
                // secret and passes through like any user's, so a handle
                // equals the raw response (journal:ATLAS-TRACKERS:0015).
                let mut forms = base64_forms(pair.expose().as_bytes());
                forms.extend([pair, token]);
                forms.extend(tok64);
                Ok(Credential { header, forms })
            }
            AuthRef::Pat { pat_env } => {
                let pat = get(pat_env)?;
                let header = Secret::new(format!("Bearer {}", pat.expose()));
                let mut forms = vec![pat];
                forms.extend(base64_forms(forms[0].expose().as_bytes()));
                Ok(Credential { header, forms })
            }
        }
    }

    /// The `Authorization` header value.
    pub(crate) fn header(&self) -> &Secret {
        &self.header
    }

    /// `s` with every form of the secret replaced by `[redacted]`: the
    /// header value, the token, its base64 and the `email:token` pair with
    /// its base64 (standard or URL-safe, padded or not); never the account
    /// email by itself.
    pub fn scrub(&self, s: &str) -> String {
        let mut out = s.to_string();
        for f in self.forms.iter().chain(std::iter::once(&self.header)) {
            let v = f.expose();
            if v.len() >= 4 && out.contains(v) {
                out = out.replace(v, "[redacted]");
            }
        }
        out
    }
}

/// `atlas track` usage.
pub const USAGE: &str = "usage: atlas track check
       atlas track smoke [--issue KEY] [--page ID] [--jql QUERY] [--cql QUERY]
       atlas track issue KEY
       atlas track jql QUERY [--limit N] [--fields LIST|*all]
       atlas track page ID
       atlas track cql QUERY [--limit N]
       atlas track snapshot ITEM [--status S --assignee A --summary T [--category C]]
       atlas track snapshot --all [--older-than DUR]
       atlas queue plan --from-jql QUERY [--kind K] [--priority P] [--uses LIST] [--after LIST] [--scope TEXT]
                        [--order-from N] [--lane-from key|fixed:LANE] [--limit N]
--limit counts every issue read, skipped ones included.
Project tracking: reads Jira and Confluence directly with the token named by
project_tracking.atlassian (token_env/email_env on Cloud, with an optional
confluence_token_env; pat_env on Data Center). An issue handle keeps the whole
record; a jql handle keeps only the fields asked for. Read-only: writes go
through the Atlassian MCP connector. Setup: wiki/Project-tracking.md.";

/// What `atlas track` runs with.
pub struct Ctx<'a> {
    /// `project_tracking.atlassian`, when configured.
    pub settings: Option<&'a Settings>,
    /// The index database (the handle store lives beside it).
    pub db: &'a Path,
    /// The `lean` section (handle settings).
    pub lean: Option<&'a Value>,
    /// The memory store file (queue items).
    pub store: &'a Path,
    /// Reads an environment variable.
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// `atlas track ...`.
///
/// # Errors
///
/// A message for the user (the credential scrubbed from it).
pub fn cli(args: &[String], ctx: &Ctx<'_>) -> std::result::Result<ExitCode, String> {
    let word = args.first().map_or("", String::as_str);
    if matches!(word, "" | "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if word == "snapshot" {
        return snapshot::cli(&args[1..], ctx);
    }
    let s = ctx.settings.ok_or("atlas track: no project_tracking.atlassian in the configuration (see wiki/Project-tracking.md)")?;
    let mut out = std::io::stdout().lock();
    match word {
        "check" => api::check(s, ctx, &mut out),
        "smoke" => api::smoke(s, ctx, &mut out, &args[1..]),
        "issue" | "jql" | "page" | "cql" => {
            let arg =
                args.get(1).filter(|a| !a.starts_with("--")).ok_or_else(|| format!("atlas track {word}: missing its argument\n{USAGE}"))?;
            let limit = flag(args, "--limit").map(|l| l.parse::<usize>().map_err(|_| format!("--limit {l:?}: a number"))).transpose()?;
            let fields = flag(args, "--fields");
            if fields.is_some() && word != "jql" {
                return Err(format!("atlas track {word}: --fields is for jql (an issue read keeps every field)"));
            }
            api::read_cli(s, ctx, &mut out, word, arg, &api::ReadOpts { limit, fields })
        }
        _ => Err(format!("atlas track: unknown command {word:?}\n{USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(s: &str) -> std::result::Result<Option<Settings>, String> {
        Settings::from_config(Some(&crate::json::parse(s).unwrap()))
    }

    const SENTINEL: &str = "ATATT3xFfGF0-sentinel-NOT-A-REAL-TOKEN_9f8e7d=AB12";

    #[test]
    fn config_names_variables_and_refuses_a_literal_token() {
        let ok = cfg(r#"{"atlassian": {"site": "https://example.atlassian.net/", "email_env": "ATLASSIAN_EMAIL", "token_env": "ATLASSIAN_API_TOKEN"}}"#).unwrap().unwrap();
        assert_eq!(ok.site, "https://example.atlassian.net");
        assert_eq!(ok.auth, AuthRef::Cloud { email_env: "ATLASSIAN_EMAIL".into(), token_env: "ATLASSIAN_API_TOKEN".into() });
        assert_eq!(ok.confluence_base().unwrap(), "https://example.atlassian.net/wiki");
        let pat = cfg(
            r#"{"atlassian": {"site": "https://jira.example.com", "confluence_site": "https://wiki.example.com", "pat_env": "JIRA_PAT"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(pat.auth, AuthRef::Pat { pat_env: "JIRA_PAT".into() });
        assert_eq!(pat.confluence_base().unwrap(), "https://wiki.example.com");
        for field in ["token_env", "pat_env", "email_env"] {
            let text = match field {
                "pat_env" => format!(r#"{{"atlassian": {{"site": "https://x.atlassian.net", "pat_env": "{SENTINEL}"}}}}"#),
                "email_env" => {
                    format!(r#"{{"atlassian": {{"site": "https://x.atlassian.net", "email_env": "{SENTINEL}", "token_env": "T"}}}}"#)
                }
                _ => format!(r#"{{"atlassian": {{"site": "https://x.atlassian.net", "email_env": "E", "token_env": "{SENTINEL}"}}}}"#),
            };
            let e = cfg(&text).unwrap_err();
            assert!(e.contains(field) && e.contains("not hold a credential"), "{e}");
            assert!(!e.contains(SENTINEL) && !e.contains("sentinel"), "the refused value is echoed: {e}");
        }
        // A token-shaped value of upper case and digits only is still too long.
        assert!(cfg(&format!(r#"{{"atlassian": {{"site": "https://x.atlassian.net", "pat_env": "{}"}}}}"#, "A1".repeat(40))).is_err());
        // Other keys: a literal "token" key is unknown, and a site with a user is refused.
        assert!(cfg(r#"{"atlassian": {"site": "https://x.atlassian.net", "token": "abc", "pat_env": "P"}}"#)
            .unwrap_err()
            .contains("unknown field"));
        assert!(cfg(r#"{"atlassian": {"site": "https://me:pw@x.atlassian.net", "pat_env": "P"}}"#).is_err());
        assert!(cfg(r#"{"atlassian": {"site": "http://x.atlassian.net", "pat_env": "P"}}"#).is_err());
        assert!(cfg(r#"{"atlassian": {"site": "https://x.atlassian.net", "pat_env": "P", "token_env": "T", "email_env": "E"}}"#).is_err());
        assert_eq!(cfg("{}").unwrap(), None);
    }

    #[test]
    fn the_credential_is_read_per_call_and_prints_redacted() {
        let s = cfg(r#"{"atlassian": {"site": "https://x.atlassian.net", "email_env": "E", "token_env": "T"}}"#).unwrap().unwrap();
        let value = std::cell::RefCell::new("first-token-value".to_string());
        let env = |n: &str| match n {
            "E" => Some("someone@example.com".to_string()),
            "T" => Some(value.borrow().clone()),
            _ => None,
        };
        let a = Credential::resolve(&s.auth, &env).unwrap();
        *value.borrow_mut() = "second-token-value".into();
        let b = Credential::resolve(&s.auth, &env).unwrap();
        assert_ne!(a.header().expose(), b.header().expose(), "the variable is read at call time, not cached");
        assert_eq!(a.header().expose(), format!("Basic {}", base64(b"someone@example.com:first-token-value")));
        assert_eq!(format!("{a:?} {:?}", a.header()), "Credential([redacted]) [redacted]");
        let msg = format!("x {} y {} z", b.header().expose(), "second-token-value");
        let clean = b.scrub(&msg);
        assert!(!clean.contains("second-token-value") && !clean.contains(&base64(b"someone@example.com:second-token-value")), "{clean}");
        let none = |_: &str| None;
        assert_eq!(Credential::resolve(&s.auth, &none).unwrap_err().to_string(), "the environment variable E is not set (or empty)");
        assert!(s.env_present(&env) && !s.env_present(&none));
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (i, o) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn the_docs_leave_secret_managers_to_the_harness() {
        let wiki = include_str!("../../wiki/Project-tracking.md");
        assert!(wiki.contains("arn:aws:secretsmanager:"), "one ARN example");
        assert!(wiki.contains("the harness"), "resolving an ARN is the harness's job");
        for cmd in ["atlas track check", "atlas track smoke --issue"] {
            assert!(wiki.contains(cmd), "{cmd} documented");
        }
        assert!(wiki.contains("pat_env") && wiki.contains("token_env"), "Cloud and Data Center setup");
    }
}
