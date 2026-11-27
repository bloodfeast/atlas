//! The read-only client: the endpoints it may call ([`ENDPOINTS`]), the
//! polite request loop ([`Client::get`]: per-site pacing, `Retry-After`,
//! capped backoff, redirects that drop the credential when they leave the
//! origin), the reads with their pagination, and the `check`, `smoke` and
//! read commands.

use std::collections::HashMap;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::http::{Curl, Method, Request, Transport};
use super::{digest, AuthRef, Credential, Ctx, HostClass, Missing, Settings};
use crate::json::{self, Value};

/// Which product an endpoint belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    /// Jira.
    Jira,
    /// Confluence.
    Confluence,
}

/// One endpoint the client may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// Its name.
    pub name: &'static str,
    /// Its method: always [`Method::Get`].
    pub method: Method,
    /// Its product.
    pub product: Product,
    /// Its path on Cloud (Confluence's below `/wiki`).
    pub cloud: &'static str,
    /// Its path on Data Center and Server.
    pub dc: &'static str,
}

const fn get(name: &'static str, product: Product, cloud: &'static str, dc: &'static str) -> Endpoint {
    Endpoint { name, method: Method::Get, product, cloud, dc }
}

/// Every endpoint the client may call. All reads; nothing that creates,
/// edits, transitions, comments or deletes.
pub const ENDPOINTS: &[Endpoint] = &[
    get("myself", Product::Jira, "/rest/api/3/myself", "/rest/api/2/myself"),
    get("mypermissions", Product::Jira, "/rest/api/3/mypermissions", "/rest/api/2/mypermissions"),
    get("issue", Product::Jira, "/rest/api/3/issue/{key}", "/rest/api/2/issue/{key}"),
    get("jql", Product::Jira, "/rest/api/3/search/jql", "/rest/api/2/search"),
    get("page", Product::Confluence, "/api/v2/pages/{id}", "/rest/api/content/{id}"),
    get("cql", Product::Confluence, "/rest/api/search", "/rest/api/search"),
    get("confluence_user", Product::Confluence, "/rest/api/user/current", "/rest/api/user/current"),
];

fn ep(name: &str) -> &'static Endpoint {
    ENDPOINTS.iter().find(|e| e.name == name).expect("a listed endpoint")
}

impl Endpoint {
    /// The path template for this deployment.
    pub fn path(&self, cloud: bool) -> &'static str {
        if cloud {
            self.cloud
        } else {
            self.dc
        }
    }

    /// How errors name it: the method and the path template, never the
    /// request (no key, query or header).
    pub fn shown(&self, cloud: bool) -> String {
        let pre = if cloud && self.product == Product::Confluence { "/wiki" } else { "" };
        format!("{} {pre}{}", self.method.name(), self.path(cloud))
    }
}

/// A read that failed: the endpoint and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerError {
    /// [`Endpoint::shown`].
    pub endpoint: String,
    /// What happened (an HTTP status, a transport failure).
    pub what: String,
    /// The HTTP status, when one came back.
    pub status: Option<u16>,
}

impl std::fmt::Display for TrackerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.endpoint, self.what)
    }
}

/// A read that worked.
#[derive(Debug, Clone)]
pub struct Got {
    /// The response (pages merged).
    pub value: Value,
    /// Bytes received, all pages.
    pub bytes: usize,
    /// Wall time, all pages, in milliseconds.
    pub ms: u128,
    /// Pages fetched.
    pub pages: usize,
    /// The endpoint, as errors name it.
    pub endpoint: String,
}

/// Percent-encodes a query value or path segment.
pub fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// `scheme://host[:port]`, lower case.
pub fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    format!("{}://{}", scheme.to_ascii_lowercase(), auth.to_ascii_lowercase())
}

/// A redirect's target: an absolute URL, or a path on the same origin.
fn resolve(from: &str, loc: &str) -> Option<String> {
    if loc.starts_with("https://") || loc.starts_with("http://") {
        Some(loc.to_string())
    } else if loc.starts_with('/') && !loc.starts_with("//") {
        Some(format!("{}{loc}", origin(from)))
    } else {
        None
    }
}

/// When each site may next be asked (per process).
static PACE: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Waits until `origin` may be asked again ([`Settings::min_interval_ms`]):
/// reads and writes share one clock per site.
pub(super) fn pace_site(s: &Settings, origin: &str, sleep: &dyn Fn(Duration)) {
    let interval = Duration::from_millis(s.min_interval_ms);
    let wait = {
        let mut g = PACE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let m = g.get_or_insert_with(HashMap::new);
        let now = Instant::now();
        let next = m.get(origin).map_or(now, |t| *t + interval);
        m.insert(origin.to_string(), now.max(next));
        next.saturating_duration_since(now)
    };
    if !wait.is_zero() {
        sleep(wait);
    }
}

/// Reads an environment variable.
type EnvFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The read-only client for one command.
pub struct Client<'a> {
    s: &'a Settings,
    cred: Credential,
    /// Confluence's own credential, when configured: given
    /// ([`Client::with_confluence`]) or read on the first Confluence
    /// request ([`Client::with_confluence_env`]).
    conf: std::cell::OnceCell<Credential>,
    /// Where Confluence's own credential is read from, when not yet read.
    conf_src: Option<(&'a AuthRef, EnvFn<'a>)>,
    t: &'a dyn Transport,
    sleep: &'a dyn Fn(Duration),
    /// Debug lines to stderr (`ATLAS_TRACK_DEBUG`), the header redacted.
    pub debug: bool,
    /// The largest response accepted, in bytes.
    pub max_bytes: u64,
    /// Every debug line written, for the tests.
    pub debug_log: std::cell::RefCell<Vec<String>>,
}

/// The largest response accepted by default: 64 MB.
pub const MAX_RESPONSE: u64 = 64 << 20;

impl<'a> Client<'a> {
    /// A client over `t` with the credential just read.
    pub fn new(s: &'a Settings, cred: Credential, t: &'a dyn Transport, sleep: &'a dyn Fn(Duration)) -> Client<'a> {
        Client {
            s,
            cred,
            conf: std::cell::OnceCell::new(),
            conf_src: None,
            t,
            sleep,
            debug: false,
            max_bytes: MAX_RESPONSE,
            debug_log: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The client with Confluence's own credential: Confluence requests
    /// carry it, Jira requests keep the first.
    #[must_use]
    pub fn with_confluence(self, cred: Credential) -> Client<'a> {
        let _ = self.conf.set(cred);
        self
    }

    /// The client with Confluence's own credential read from `env` on the
    /// first Confluence request, so a Jira read never needs Confluence's
    /// variable.
    #[must_use]
    pub fn with_confluence_env(mut self, auth: &'a AuthRef, env: &'a dyn Fn(&str) -> Option<String>) -> Client<'a> {
        self.conf_src = Some((auth, env));
        self
    }

    /// The credential an endpoint's requests carry.
    fn cred_for(&self, product: Product) -> Result<&Credential, Missing> {
        if product == Product::Confluence {
            if let Some(c) = self.conf.get() {
                return Ok(c);
            }
            if let Some((auth, env)) = self.conf_src {
                let c = Credential::resolve(auth, env)?;
                return Ok(self.conf.get_or_init(|| c));
            }
        }
        Ok(&self.cred)
    }

    fn cloud(&self) -> bool {
        self.s.auth.is_cloud()
    }

    /// Scrubs `s` of the credential.
    pub fn scrub(&self, s: &str) -> String {
        let s = self.cred.scrub(s);
        match self.conf.get() {
            Some(c) => c.scrub(&s),
            None => s,
        }
    }

    fn debug_line(&self, line: &str) {
        if self.debug {
            let l = self.scrub(line);
            eprintln!("atlas track: {l}");
            self.debug_log.borrow_mut().push(l);
        }
    }

    /// Waits until `origin` may be asked again ([`Settings::min_interval_ms`]).
    fn pace(&self, origin: &str) {
        pace_site(self.s, origin, self.sleep);
    }

    fn err(&self, endpoint: &str, what: &str, status: Option<u16>) -> TrackerError {
        TrackerError { endpoint: endpoint.to_string(), what: self.scrub(what), status }
    }

    /// One GET, its redirects and retries.
    ///
    /// # Errors
    ///
    /// A [`TrackerError`] naming the endpoint and the status or failure.
    pub fn get(&self, e: &Endpoint, base: &str, path_q: &str) -> Result<Got, TrackerError> {
        let shown = e.shown(self.cloud());
        let mut url = format!("{base}{path_q}");
        let mut auth = Some(self.cred_for(e.product).map_err(|m| self.err(&shown, &m.to_string(), None))?.header());
        let start = Instant::now();
        let (mut tries, mut hops) = (0u32, 0u32);
        loop {
            self.pace(&origin(&url));
            self.debug_line(&format!("> {} {url}", e.method.name()));
            self.debug_line(if auth.is_some() { "> Authorization: [redacted]" } else { "> (no Authorization: another origin)" });
            let req = Request { method: e.method, write: None, url: &url, auth, timeout: self.s.timeout(), max_bytes: self.max_bytes };
            let r = self.t.send(&req).map_err(|f| self.err(&shown, &f.describe(self.s.timeout()), None))?;
            self.debug_line(&format!("< {} ({} bytes)", r.status, r.body.len()));
            match r.status {
                // A 200 the site answered as an anonymous user: the login was
                // refused, so the empty result is an auth failure, not an
                // answer (JRACLOUD-96181; a scoped token on the site URL).
                200..=299 if r.header("x-seraph-loginreason").is_some_and(|v| v.contains("AUTHENTICATED_FAILED")) => {
                    return Err(self.err(
                        &shown,
                        &format!(
                            "HTTP {} with x-seraph-loginreason AUTHENTICATED_FAILED, counted as 401 (unauthorized: the site refused the credential and answered anonymously)",
                            r.status
                        ),
                        Some(401),
                    ));
                }
                200..=299 => {
                    let text = std::str::from_utf8(&r.body).map_err(|_| {
                        self.err(&shown, &format!("HTTP {}: a {}-byte response that is not UTF-8", r.status, r.body.len()), Some(r.status))
                    })?;
                    let value = json::parse(text).map_err(|_| {
                        self.err(&shown, &format!("HTTP {}: malformed JSON in a {}-byte response", r.status, r.body.len()), Some(r.status))
                    })?;
                    return Ok(Got { value, bytes: r.body.len(), ms: start.elapsed().as_millis(), pages: 1, endpoint: shown });
                }
                301 | 302 | 303 | 307 | 308 => {
                    hops += 1;
                    if hops > 5 {
                        return Err(self.err(&shown, &format!("HTTP {}: more than 5 redirects", r.status), Some(r.status)));
                    }
                    let next = r
                        .header("location")
                        .and_then(|l| resolve(&url, l))
                        .filter(|n| crate::models::url_allowed(n))
                        // An https site never steps down to plain http, not even loopback.
                        .filter(|n| !url.starts_with("https://") || n.starts_with("https://"));
                    let Some(next) = next else {
                        return Err(self.err(
                            &shown,
                            &format!("HTTP {}: a redirect atlas does not follow (no Location, or not https)", r.status),
                            Some(r.status),
                        ));
                    };
                    if origin(&next) != origin(&url) {
                        // Leaving the origin: the credential stays behind,
                        // for this hop and every later one.
                        auth = None;
                    }
                    url = next;
                }
                429 | 500..=599 if tries < self.s.max_retries => {
                    let backoff = Duration::from_millis(self.s.backoff_ms.saturating_mul(1 << tries.min(16)));
                    let asked = r.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()).map(Duration::from_secs);
                    let wait = asked.unwrap_or(backoff).min(Duration::from_secs(self.s.max_wait_s));
                    self.debug_line(&format!("  retry {} after {} ms", tries + 1, wait.as_millis()));
                    (self.sleep)(wait);
                    tries += 1;
                }
                st => {
                    let why = match st {
                        400 => "bad request: check the JQL or CQL (Cloud refuses unbounded JQL)".to_string(),
                        // The gateway is the fix only for Cloud auth on a
                        // site URL; a PAT or the gateway itself gets none.
                        401 => format!(
                            "unauthorized: check the credential's environment variables{}",
                            if self.cloud() && HostClass::of(base) == HostClass::Cloud {
                                "; a scoped token needs the api.atlassian.com gateway URL"
                            } else {
                                ""
                            }
                        ),
                        403 => "forbidden: the account or token may not read this".to_string(),
                        404 => "not found, or not visible to this account".to_string(),
                        429 => format!("rate limited; gave up after {tries} retries"),
                        500..=599 => format!("server error; gave up after {tries} retries"),
                        _ => "unexpected status".to_string(),
                    };
                    return Err(self.err(&shown, &format!("HTTP {st} ({why})"), Some(st)));
                }
            }
        }
    }

    /// The signed-in Jira account.
    ///
    /// # Errors
    ///
    /// As [`Client::get`].
    pub fn myself(&self) -> Result<Got, TrackerError> {
        let e = ep("myself");
        self.get(e, self.s.jira_base(), e.path(self.cloud()))
    }

    /// The write permissions the Jira account holds anywhere.
    ///
    /// # Errors
    ///
    /// As [`Client::get`].
    pub fn permissions(&self) -> Result<Got, TrackerError> {
        let e = ep("mypermissions");
        self.get(e, self.s.jira_base(), &format!("{}?permissions={}", e.path(self.cloud()), enc(&WRITE_PERMISSIONS.join(","))))
    }

    /// The Confluence account and the operations it may do.
    ///
    /// # Errors
    ///
    /// As [`Client::get`].
    pub fn confluence_user(&self) -> Result<Got, TrackerError> {
        let e = ep("confluence_user");
        self.get(e, &self.confluence_base(e)?, &format!("{}?expand=operations", e.path(self.cloud())))
    }

    /// One issue, every navigable field.
    ///
    /// # Errors
    ///
    /// As [`Client::get`]; a key that is not `A-Z 0-9 _ -` is refused first.
    pub fn issue(&self, key: &str) -> Result<Got, TrackerError> {
        let e = ep("issue");
        if key.is_empty() || key.len() > 64 || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(self.err(&e.shown(self.cloud()), "an issue key is letters, digits, - and _", None));
        }
        self.get(e, self.s.jira_base(), &e.path(self.cloud()).replace("{key}", &enc(key)))
    }

    /// A JQL search, following its pages up to `limit` issues.
    ///
    /// # Errors
    ///
    /// As [`Client::get`].
    pub fn jql(&self, q: &str, limit: usize) -> Result<Got, TrackerError> {
        self.jql_fields(q, limit, None)
    }

    /// [`Client::jql`] asking for `fields` (a comma list, `*all` or
    /// `*navigable`) in place of the six the digest shows.
    ///
    /// # Errors
    ///
    /// As [`Client::get`]; a field list with other than letters, digits and
    /// `_ - . * ,` is refused first.
    pub fn jql_fields(&self, q: &str, limit: usize, fields: Option<&str>) -> Result<Got, TrackerError> {
        let e = ep("jql");
        let limit = limit.clamp(1, MAX_ITEMS);
        let fields = match fields {
            None => DEFAULT_JQL_FIELDS.to_string(),
            Some(f) if fields_ok(f) => enc(f),
            Some(_) => return Err(self.err(&e.shown(self.cloud()), "--fields is a comma list of field ids, *all or *navigable", None)),
        };
        let mut acc: Option<Got> = None;
        let mut token: Option<String> = None;
        loop {
            let have = acc.as_ref().map_or(0, |a| count(&a.value, "issues"));
            let page = (limit - have).min(100);
            let path = if self.cloud() {
                let mut p = format!("{}?jql={}&maxResults={page}&fields={fields}", e.cloud, enc(q));
                if let Some(t) = &token {
                    p.push_str(&format!("&nextPageToken={}", enc(t)));
                }
                p
            } else {
                format!("{}?jql={}&startAt={have}&maxResults={page}&fields={fields}", e.dc, enc(q))
            };
            let got = self.get(e, self.s.jira_base(), &path)?;
            token = got.value.get("nextPageToken").and_then(Value::as_str).map(str::to_string);
            let n = count(&got.value, "issues");
            let total = got.value.get("total").and_then(Value::as_u64);
            let last = got.value.get("isLast").and_then(Value::as_bool);
            acc = Some(merge(acc, got, "issues"));
            let have = have + n;
            let more = if self.cloud() { last != Some(true) && token.is_some() } else { total.is_some_and(|t| (have as u64) < t) };
            if !more || n == 0 || have >= limit {
                break;
            }
        }
        Ok(acc.expect("one page at least"))
    }

    /// One Confluence page with its body (storage format).
    ///
    /// # Errors
    ///
    /// As [`Client::get`]; an id that is not digits is refused first.
    pub fn page(&self, id: &str) -> Result<Got, TrackerError> {
        let e = ep("page");
        if id.is_empty() || id.len() > 32 || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(self.err(&e.shown(self.cloud()), "a page id is digits", None));
        }
        let q = if self.cloud() { "?body-format=storage" } else { "?expand=body.storage,version,space" };
        self.get(e, &self.confluence_base(e)?, &format!("{}{q}", e.path(self.cloud()).replace("{id}", id)))
    }

    /// Confluence's base URL, or the error naming the missing
    /// `confluence_site` (never Jira's gateway with `/wiki` added).
    fn confluence_base(&self, e: &Endpoint) -> Result<String, TrackerError> {
        self.s.confluence_base().map_err(|m| self.err(&e.shown(self.cloud()), &m, None))
    }

    /// A CQL search, following `_links.next` up to `limit` hits. A next
    /// link must be a search path on the configured site.
    ///
    /// # Errors
    ///
    /// As [`Client::get`].
    pub fn cql(&self, q: &str, limit: usize) -> Result<Got, TrackerError> {
        let e = ep("cql");
        let limit = limit.clamp(1, MAX_ITEMS);
        let base = self.confluence_base(e)?;
        let mut path = format!("{}?cql={}&limit={}", e.path(self.cloud()), enc(q), limit.min(50));
        let mut acc: Option<Got> = None;
        loop {
            let got = self.get(e, &base, &path)?;
            let n = count(&got.value, "results");
            let next = got.value.pointer("/_links/next").and_then(Value::as_str).map(str::to_string);
            acc = Some(merge(acc, got, "results"));
            let have = acc.as_ref().map_or(0, |a| count(&a.value, "results"));
            match next {
                Some(n2) if n > 0 && have < limit && n2.starts_with(&format!("{}?", e.path(self.cloud()))) => path = n2,
                _ => break,
            }
        }
        Ok(acc.expect("one page at least"))
    }
}

/// The fields a JQL search asks for unless `--fields` names others: the
/// ones its digest rows show.
pub const DEFAULT_JQL_FIELDS: &str = "summary,status,issuetype,priority,assignee,updated";

/// True for a `--fields` value: field ids (letters, digits, `_`, `-`, `.`),
/// `*all` or `*navigable`, comma separated, at most 2000 bytes.
pub fn fields_ok(f: &str) -> bool {
    !f.is_empty()
        && f.len() <= 2000
        && f.split(',').all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'*')))
}

/// The most issues or hits one read follows pages for.
pub const MAX_ITEMS: usize = 1000;

/// The Jira permissions `check` asks about.
pub const WRITE_PERMISSIONS: &[&str] = &["CREATE_ISSUES", "EDIT_ISSUES", "TRANSITION_ISSUES", "ADD_COMMENTS", "DELETE_ISSUES"];

fn count(v: &Value, key: &str) -> usize {
    v.get(key).and_then(Value::as_array).map_or(0, Vec::len)
}

/// Appends a page's `key` array to the pages so far; the newest page's
/// cursor fields win.
fn merge(acc: Option<Got>, got: Got, key: &str) -> Got {
    let Some(mut a) = acc else { return got };
    let mut v = got.value;
    let items = v
        .as_object_mut()
        .and_then(|m| m.get_mut(key))
        .map(Value::take)
        .and_then(|x| if let Value::Array(x) = x { Some(x) } else { None })
        .unwrap_or_default();
    if let Some(Value::Array(dst)) = a.value.as_object_mut().and_then(|m| m.get_mut(key)) {
        dst.extend(items);
    }
    if let (Some(dm), Some(sm)) = (a.value.as_object_mut(), v.as_object_mut()) {
        for k in ["nextPageToken", "isLast", "_links", "size", "start"] {
            match sm.remove(k) {
                Some(x) => {
                    dm.insert(k.to_string(), x);
                }
                None => {
                    dm.remove(k);
                }
            }
        }
    }
    a.bytes += got.bytes;
    a.ms += got.ms;
    a.pages += 1;
    a
}

fn real_sleep(d: Duration) {
    std::thread::sleep(d);
}

/// A client for a command, Jira's credential read now and Confluence's own
/// on its first request.
fn client<'a>(s: &'a Settings, ctx: &'a Ctx<'_>, t: &'a dyn Transport) -> Result<Client<'a>, String> {
    let cred = Credential::resolve(&s.auth, ctx.env).map_err(|m| format!("atlas track: {m}"))?;
    let mut c = Client::new(s, cred, t, &real_sleep);
    if let Some(a) = &s.confluence_auth {
        c = c.with_confluence_env(a, ctx.env);
    }
    c.debug = (ctx.env)("ATLAS_TRACK_DEBUG").is_some_and(|v| !v.is_empty() && v != "0");
    Ok(c)
}

fn w(out: &mut dyn Write, s: &str) -> Result<(), String> {
    writeln!(out, "{s}").map_err(|e| e.to_string())
}

/// `atlas track check`.
///
/// # Errors
///
/// A write error.
pub fn check(s: &Settings, ctx: &Ctx<'_>, out: &mut dyn Write) -> Result<ExitCode, String> {
    let curl = Curl::default();
    check_with(s, ctx, out, &curl)
}

/// [`check`] over a transport.
///
/// # Errors
///
/// A write error.
pub fn check_with(s: &Settings, ctx: &Ctx<'_>, out: &mut dyn Write, t: &dyn Transport) -> Result<ExitCode, String> {
    let kind = match (s.auth.is_cloud(), s.confluence_auth.is_some()) {
        (true, true) => "Cloud, basic auth (email_env + token_env for Jira, confluence_token_env for Confluence)",
        (true, false) => "Cloud, basic auth (email_env + token_env)",
        _ => "Data Center/Server, bearer PAT (pat_env)",
    };
    w(out, &format!("config: project_tracking.atlassian ok; site is {}; {kind}", s.host_class()))?;
    let mut all = true;
    for n in s.env_names() {
        let present = (ctx.env)(n).is_some_and(|v| !v.is_empty());
        all &= present;
        w(out, &format!("env {n}: {}", if present { "present" } else { "absent" }))?;
    }
    if !all {
        w(out, "auth: not tried (a variable is absent)")?;
        return Ok(ExitCode::FAILURE);
    }
    let c = client(s, ctx, t)?;
    let mut ok = true;
    // A scoped token on the site URL is refused there: it works only
    // through the gateway (issue #304).
    let site_host = s.is_cloud_site();
    let gateway_hint = |product: &str| {
        let set = if product == "jira" {
            // Both products move: a Jira gateway site with no
            // confluence_site leaves Confluence nowhere to go.
            "set site to https://api.atlassian.com/ex/jira/<cloud id>, and confluence_site to https://api.atlassian.com/ex/confluence/<cloud id>/wiki with confluence_token_env naming a token with Confluence's scopes (a scoped token holds one product's scopes)"
        } else {
            "set confluence_site to https://api.atlassian.com/ex/confluence/<cloud id>/wiki"
        };
        format!(
            "hint: a scoped API token works only through Atlassian's gateway: {set} (the cloud id is at https://<your-site>.atlassian.net/_edge/tenant_info); a classic token works on the site URL"
        )
    };
    let jira_auth_failed = match c.myself() {
        Ok(g) => {
            w(out, &format!("jira auth: ok ({} 200, {} ms)", g.endpoint, g.ms))?;
            false
        }
        Err(e) => {
            ok = false;
            w(out, &format!("jira auth: failed: {e}"))?;
            if e.status == Some(401) && s.auth.is_cloud() && site_host {
                w(out, &gateway_hint("jira"))?;
            }
            e.status == Some(401)
        }
    };
    // After a failed login the answer would be about an anonymous user, not
    // this account: never "reads only" (issue #304).
    let perms = if jira_auth_failed { None } else { Some(c.permissions()) };
    match perms {
        None => w(out, "jira writes: unknown (auth failed)")?,
        Some(Ok(g)) => {
            let held: Vec<(&str, bool)> = WRITE_PERMISSIONS
                .iter()
                .map(|p| (*p, g.value.pointer(&format!("/permissions/{p}/havePermission")).and_then(Value::as_bool).unwrap_or(false)))
                .collect();
            let list: Vec<String> = held.iter().map(|(p, h)| format!("{p} {}", if *h { "yes" } else { "no" })).collect();
            let any = held.iter().any(|(_, h)| *h);
            w(
                out,
                &format!(
                    "jira writes: {}; {}",
                    list.join(", "),
                    if any { "writes WOULD be allowed for this account" } else { "reads only" }
                ),
            )?;
        }
        Some(Err(e)) => w(out, &format!("jira writes: not known: {e}"))?,
    }
    // A Jira gateway site with no confluence_site: Confluence is not
    // configured, which a Jira-only setup may mean; say so, never guess.
    let conf = match s.confluence_base() {
        Err(m) => {
            w(out, &format!("confluence: not tried: {m}"))?;
            None
        }
        Ok(_) => Some(c.confluence_user()),
    };
    match conf {
        None => {}
        Some(Ok(g)) => {
            // Fail-safe: a verb that is not a known read counts as a
            // possible write, so an unknown verb never makes the account
            // look read-only; the known writes are named as such.
            let (mut ops, mut unknown, mut other) = (Vec::new(), Vec::new(), Vec::new());
            let listed = g.value.get("operations").and_then(Value::as_array);
            for o in listed.into_iter().flatten() {
                let (verb, target) = (digest_str(o, "operation"), digest_str(o, "targetType"));
                let x = format!("{verb} {target}");
                if CONFLUENCE_WRITES.contains(&verb) {
                    ops.push(x);
                } else if !confluence_read(verb, target) {
                    unknown.push(x);
                } else if verb != "read" {
                    other.push(x);
                }
            }
            w(out, &format!("confluence auth: ok ({} 200, {} ms)", g.endpoint, g.ms))?;
            // Absent or not a list: nothing is known, so never "reads only".
            if listed.is_none() {
                w(out, "confluence writes: not known (no operations listed)")?;
            } else if ops.is_empty() && unknown.is_empty() {
                w(out, "confluence writes: reads only")?;
            } else {
                if !ops.is_empty() {
                    w(out, &format!("confluence writes: WOULD be allowed for this account: {}", ops.join(", ")))?;
                }
                if !unknown.is_empty() {
                    w(
                        out,
                        &format!("confluence writes: possibly; operations atlas does not know, counted as writes: {}", unknown.join(", ")),
                    )?;
                }
            }
            if !other.is_empty() {
                w(out, &format!("confluence other operations (not counted as writes): {}", other.join(", ")))?;
            }
        }
        Some(Err(e)) => {
            ok = false;
            w(out, &format!("confluence auth: failed: {e}"))?;
            w(out, &format!("confluence writes: {}", if e.status == Some(401) { "unknown (auth failed)" } else { "not known" }))?;
            let conf_host = s.confluence_site.as_ref().map_or(site_host, |c| HostClass::of(c) == HostClass::Cloud);
            if e.status == Some(401) && s.confluence_auth().is_cloud() && conf_host {
                w(out, &gateway_hint("confluence"))?;
            }
        }
    }
    w(out, &super::write::check_line(s, ctx, false))?;
    w(out, &super::write::check_line(s, ctx, true))?;
    w(out, "note: these are the account's permissions; a scoped token's scopes are not visible to the API, so a read-only scoped token still shows the account's writes here and its writes fail with 400, 401 or 404. Reads are GET only; writes (atlas track comment, transition, edit, page-update) show their request and send nothing without --apply.")?;
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// True for a Confluence operation known not to write: `read` on anything,
/// and `use application` (log in), which prints as "other". Anything else,
/// `use` on another target included, counts as a possible write.
fn confluence_read(verb: &str, target: &str) -> bool {
    verb == "read" || (verb == "use" && target == "application")
}

/// The Confluence operation verbs known to write, named as writes by
/// `check`.
const CONFLUENCE_WRITES: [&str; 9] = ["create", "update", "delete", "administer", "archive", "restore", "move", "copy", "export"];

fn digest_str<'v>(v: &'v Value, k: &str) -> &'v str {
    v.get(k).and_then(Value::as_str).unwrap_or("?")
}

/// The digest of a read, a panic in it caught.
fn safe_digest(id: &str, label: &str, text: &str) -> Result<String, String> {
    let r = std::panic::catch_unwind(|| {
        crate::digest::digest(id, label, crate::digest::Shape::Tracker, text, None, crate::digest::DEFAULT_MAX_BYTES)
    });
    r.map_err(|_| format!("atlas track: the digest failed; the raw response is kept as {id} (recall {id} lines=1-80)"))
}

fn toks(bytes: usize) -> String {
    format!("~{:.0} tok", bytes as f64 / crate::tokens::DEFAULT_BYTES_PER_TOKEN)
}

/// Field names with a value (names only, never values).
fn set_keys(v: Option<&Value>) -> (usize, Vec<String>) {
    let Some(m) = v.and_then(Value::as_object) else { return (0, Vec::new()) };
    let mut ks: Vec<String> = m.iter().filter(|(_, x)| !x.is_null()).map(|(k, _)| k.clone()).collect();
    ks.sort();
    (m.len(), ks)
}

/// `atlas track smoke`: each read's shape and size, never its content.
///
/// # Errors
///
/// A write error, or no read asked for.
pub fn smoke(s: &Settings, ctx: &Ctx<'_>, out: &mut dyn Write, args: &[String]) -> Result<ExitCode, String> {
    let curl = Curl::default();
    smoke_with(s, ctx, out, args, &curl)
}

/// [`smoke`] over a transport.
///
/// # Errors
///
/// As [`smoke`].
pub fn smoke_with(s: &Settings, ctx: &Ctx<'_>, out: &mut dyn Write, args: &[String], t: &dyn Transport) -> Result<ExitCode, String> {
    let flag = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let (issue, page, jql, cql) = (flag("--issue"), flag("--page"), flag("--jql"), flag("--cql"));
    if issue.is_none() && page.is_none() && jql.is_none() && cql.is_none() {
        return Err(format!("atlas track smoke: give at least one of --issue KEY, --page ID, --jql QUERY, --cql QUERY\n{}", super::USAGE));
    }
    let c = client(s, ctx, t)?;
    let mut ok = true;
    let mut line =
        |out: &mut dyn Write, what: &str, r: Result<Got, TrackerError>, detail: &dyn Fn(&Value) -> Vec<String>| -> Result<(), String> {
            match r {
                Ok(g) => {
                    let text = c.scrub(&digest::stored_text(&g.value));
                    let d = safe_digest("h:0000000000", what, &text)?;
                    let ratio = g.bytes as f64 / d.len().max(1) as f64;
                    w(
                        out,
                        &format!(
                            "{what}: {} {} page(s) in {} ms; raw {} B ({}), digest {} B ({}), {ratio:.1}x",
                            g.endpoint,
                            g.pages,
                            g.ms,
                            g.bytes,
                            toks(g.bytes),
                            d.len(),
                            toks(d.len())
                        ),
                    )?;
                    for l in detail(&g.value) {
                        w(out, &format!("  {l}"))?;
                    }
                }
                Err(e) => {
                    ok = false;
                    w(out, &format!("{what}: failed: {e}"))?;
                }
            }
            Ok(())
        };
    if let Some(k) = issue {
        line(out, "issue", c.issue(&k), &|v| {
            let f = v.get("fields");
            let (n, set) = set_keys(f);
            let desc = match f.and_then(|f| f.get("description")) {
                Some(Value::Object(_)) => "adf",
                Some(Value::String(_)) => "string",
                _ => "none",
            };
            let cnt = |p: &str| v.pointer(p).and_then(Value::as_array).map_or(0, Vec::len);
            vec![
                format!("fields {n}, non-null {}: {}", set.len(), set.join(", ")),
                format!(
                    "description {desc}, comments {}, links {}, attachments {}",
                    cnt("/fields/comment/comments"),
                    cnt("/fields/issuelinks"),
                    cnt("/fields/attachment")
                ),
            ]
        })?;
    }
    if let Some(q) = jql {
        line(out, "jql", c.jql(&q, 50), &|v| {
            let is = v.get("issues").and_then(Value::as_array);
            let (_, set) = set_keys(is.and_then(|a| a.first()).and_then(|i| i.get("fields")));
            vec![format!(
                "issues {}, isLast {}, issue fields: {}",
                is.map_or(0, Vec::len),
                v.get("isLast").map_or("absent".to_string(), json::to_string),
                set.join(", ")
            )]
        })?;
    }
    if let Some(id) = page {
        line(out, "page", c.page(&id), &|v| {
            let raw = v.pointer("/body/storage/value").and_then(Value::as_str).map_or(0, str::len);
            let md = digest::page_body_md(v).unwrap_or_default();
            let heads = md.lines().filter(|l| l.starts_with('#')).count();
            let (_, keys) = set_keys(Some(v));
            vec![format!("body storage {raw} B -> markdown {} B, headings {heads}; keys: {}", md.len(), keys.join(", "))]
        })?;
    }
    if let Some(q) = cql {
        line(out, "cql", c.cql(&q, 50), &|v| {
            let rs = v.get("results").and_then(Value::as_array);
            let (_, keys) = set_keys(rs.and_then(|a| a.first()));
            vec![format!(
                "results {}, totalSize {}, result keys: {}",
                rs.map_or(0, Vec::len),
                if v.get("totalSize").is_some() { "present" } else { "absent" },
                keys.join(", ")
            )]
        })?;
    }
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// `atlas track issue|jql|page|cql`: the digest, the raw response kept
/// as a handle.
///
/// # Errors
///
/// A read or store failure (the credential scrubbed out).
pub fn read_cli(s: &Settings, ctx: &Ctx<'_>, out: &mut dyn Write, word: &str, arg: &str, opts: &ReadOpts) -> Result<ExitCode, String> {
    let curl = Curl::default();
    read_with(s, ctx, out, word, arg, opts, &curl, crate::handles::HintForm::for_stdout())
}

/// A read's options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadOpts {
    /// `--limit N`: the most issues or hits (default 50).
    pub limit: Option<usize>,
    /// `--fields LIST`: the fields a JQL search asks for (default
    /// [`DEFAULT_JQL_FIELDS`]).
    pub fields: Option<String>,
}

/// [`read_cli`] over a transport, its hints (the digest's and the digest
/// failure's) written in `form`: the command on a terminal, `recall`
/// otherwise.
///
/// # Errors
///
/// As [`read_cli`].
#[allow(clippy::too_many_arguments)]
pub fn read_with(
    s: &Settings,
    ctx: &Ctx<'_>,
    out: &mut dyn Write,
    word: &str,
    arg: &str,
    opts: &ReadOpts,
    t: &dyn Transport,
    form: crate::handles::HintForm,
) -> Result<ExitCode, String> {
    let c = client(s, ctx, t)?;
    let limit = opts.limit.unwrap_or(50);
    let (got, label) = match word {
        "issue" => (c.issue(arg), format!("jira issue {arg}")),
        "jql" => (
            c.jql_fields(arg, limit, opts.fields.as_deref()),
            format!("jira jql search (fields {})", opts.fields.as_deref().unwrap_or(DEFAULT_JQL_FIELDS)),
        ),
        "page" => (c.page(arg), format!("confluence page {arg}")),
        _ => (c.cql(arg, limit), "confluence cql search".to_string()),
    };
    let got = got.map_err(|e| format!("atlas track {word}: {e}"))?;
    // Scrubbed like every other output: a proxy or error page can echo the
    // request headers in a 200 body.
    let text = c.scrub(&digest::stored_text(&got.value));
    let mut h =
        crate::handles::Handles::open(ctx.db, crate::handles::HandleCfg::from_lean(ctx.lean)).map_err(|e| c.scrub(&e.to_string()))?;
    let meta = crate::handles::Meta {
        kind: "tracker".into(),
        label: label.clone(),
        cmd: got.endpoint.clone(),
        cwd: String::new(),
        session: String::new(),
        exit_code: None,
        ms: i64::try_from(got.ms).ok(),
    };
    let row = h.put(text.as_bytes(), &meta).map_err(|e| c.scrub(&e.to_string()))?;
    let d = safe_digest(&row.id, &label, &text).map_err(|e| form.apply(&e))?;
    out.write_all(form.apply(&d).as_bytes()).map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::trackers::http::{Failure, Response};
    use crate::trackers::AuthRef;
    use std::cell::RefCell;

    /// A transport that answers from a script and records each request.
    pub(crate) struct Script {
        pub answers: RefCell<Vec<Result<Response, Failure>>>,
        pub seen: RefCell<Vec<(String, String, bool, String)>>,
    }

    impl Script {
        pub(crate) fn new(a: Vec<Result<Response, Failure>>) -> Script {
            Script { answers: RefCell::new(a.into_iter().rev().collect()), seen: RefCell::new(Vec::new()) }
        }
    }

    impl Transport for Script {
        fn send(&self, r: &Request<'_>) -> Result<Response, Failure> {
            self.seen.borrow_mut().push((
                r.write.map_or(r.method.name(), |w| w.method.name()).to_string(),
                r.url.to_string(),
                r.auth.is_some(),
                crate::trackers::http::curl_config(r),
            ));
            self.answers.borrow_mut().pop().unwrap_or(Ok(ok("{}")))
        }
    }

    pub(crate) fn ok(body: &str) -> Response {
        Response { status: 200, headers: vec![], body: body.as_bytes().to_vec() }
    }

    fn status(st: u16, headers: &[(&str, &str)]) -> Response {
        Response {
            status: st,
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: b"{\"errorMessages\":[\"x\"]}".to_vec(),
        }
    }

    pub(crate) fn settings(site: &str, cloud: bool) -> Settings {
        Settings {
            write: Default::default(),
            site: site.into(),
            confluence_site: None,
            auth: if cloud {
                AuthRef::Cloud { email_env: "E".into(), token_env: "T".into() }
            } else {
                AuthRef::Pat { pat_env: "P".into() }
            },
            confluence_auth: None,
            timeout_s: 5,
            min_interval_ms: 0,
            max_retries: 2,
            max_wait_s: 3,
            backoff_ms: 100,
        }
    }

    fn env(n: &str) -> Option<String> {
        match n {
            "E" => Some("e@example.com".into()),
            "T" | "P" => Some("tok-SENTINEL-value".into()),
            _ => None,
        }
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/atlassian").join(name)).unwrap()
    }

    #[test]
    fn the_client_sends_only_get_to_read_endpoints() {
        // The paths a read may use: anything else (a transition, a comment
        // post, an edit) is not in this list.
        let reads = [
            "/rest/api/3/myself",
            "/rest/api/2/myself",
            "/rest/api/3/mypermissions",
            "/rest/api/2/mypermissions",
            "/rest/api/3/issue/{key}",
            "/rest/api/2/issue/{key}",
            "/rest/api/3/search/jql",
            "/rest/api/2/search",
            "/api/v2/pages/{id}",
            "/rest/api/content/{id}",
            "/rest/api/search",
            "/rest/api/user/current",
        ];
        for e in ENDPOINTS {
            assert_eq!(e.method, Method::Get, "{} is not a GET", e.name);
            assert!(reads.contains(&e.cloud) && reads.contains(&e.dc), "{} is not a read path", e.name);
        }
        for cloud in [true, false] {
            let s = settings("https://ro.example.atlassian.net", cloud);
            let t = Script::new(vec![]);
            let sl = |_: Duration| {};
            let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
            c.myself().unwrap();
            c.permissions().unwrap();
            c.confluence_user().unwrap();
            c.issue("PROJ-1").unwrap();
            c.jql("project = PROJ", 10).unwrap();
            c.page("123").unwrap();
            c.cql("type = page", 10).unwrap();
            let seen = t.seen.borrow();
            assert_eq!(seen.len(), 7);
            for (m, url, _, cfg) in seen.iter() {
                assert_eq!(m, "GET", "{url}");
                assert!(cfg.contains("request = GET\n") && !cfg.contains("data") && !cfg.contains("upload"), "{cfg}");
                let path = url.trim_start_matches(s.site.as_str()).trim_start_matches("/wiki").split('?').next().unwrap();
                assert!(
                    ENDPOINTS.iter().any(|e| {
                        let tpl = e.path(cloud);
                        let pre = tpl.split('{').next().unwrap();
                        if tpl.contains('{') {
                            path.starts_with(pre)
                        } else {
                            path == tpl
                        }
                    }),
                    "{path} is not listed"
                );
            }
        }
    }

    #[test]
    fn retry_after_is_honoured_and_capped_and_5xx_backs_off() {
        let s = settings("https://retry.example.atlassian.net", true);
        let t =
            Script::new(vec![Ok(status(429, &[("retry-after", "2")])), Ok(status(429, &[("retry-after", "3600")])), Ok(ok("{\"a\":1}"))]);
        let waits = RefCell::new(Vec::new());
        let sl = |d: Duration| waits.borrow_mut().push(d);
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        c.myself().unwrap();
        assert_eq!(*waits.borrow(), vec![Duration::from_secs(2), Duration::from_secs(3)], "Retry-After honoured, and capped at max_wait_s");
        let t = Script::new(vec![Ok(status(503, &[])), Ok(status(502, &[])), Ok(status(500, &[]))]);
        waits.borrow_mut().clear();
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        let e = c.myself().unwrap_err();
        assert_eq!(*waits.borrow(), vec![Duration::from_millis(100), Duration::from_millis(200)], "doubling backoff, max_retries 2");
        assert_eq!(e.to_string(), "GET /rest/api/3/myself: HTTP 500 (server error; gave up after 2 retries)");
    }

    #[test]
    fn requests_are_rate_limited_per_site() {
        let mut s = settings("https://pace-a.example.atlassian.net", true);
        s.min_interval_ms = 400;
        let t = Script::new(vec![]);
        let waits = RefCell::new(Vec::new());
        let sl = |d: Duration| waits.borrow_mut().push(d);
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        c.myself().unwrap();
        c.myself().unwrap();
        let s2 = Settings { site: "https://pace-b.example.atlassian.net".into(), ..s.clone() };
        let c2 = Client::new(&s2, Credential::resolve(&s2.auth, &env).unwrap(), &t, &sl);
        c2.myself().unwrap();
        let w = waits.borrow();
        assert_eq!(w.len(), 1, "only the second call to the same site waits: {w:?}");
        assert!(w[0] > Duration::from_millis(300) && w[0] <= Duration::from_millis(400), "{w:?}");
    }

    #[test]
    fn errors_name_the_status_and_endpoint_not_the_request() {
        let s = settings("https://err.example.atlassian.net", true);
        for (st, want) in [(401, "HTTP 401 (unauthorized"), (403, "HTTP 403 (forbidden"), (404, "HTTP 404 (not found")] {
            let t = Script::new(vec![Ok(status(st, &[]))]);
            let sl = |_: Duration| {};
            let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
            let e = c.issue("SECRETPROJ-77").unwrap_err().to_string();
            assert!(e.starts_with("GET /rest/api/3/issue/{key}: ") && e.contains(want), "{e}");
            assert!(!e.contains("SECRETPROJ") && !e.contains("err.example") && !e.contains("SENTINEL"), "{e}");
        }
        let t = Script::new(vec![Err(Failure::Dns)]);
        let sl = |_: Duration| {};
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        assert_eq!(
            c.cql("text ~ \"secret plan\"", 5).unwrap_err().to_string(),
            "GET /wiki/rest/api/search: the host name did not resolve (DNS)"
        );
    }

    #[test]
    fn jql_pagination_follows_next_page_token() {
        let s = settings("https://page.example.atlassian.net", true);
        let t = Script::new(vec![Ok(ok(&fixture("jira-search-1.json"))), Ok(ok(&fixture("jira-search-2.json")))]);
        let sl = |_: Duration| {};
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        let g = c.jql("project = PROJ", 50).unwrap();
        assert_eq!((g.pages, count(&g.value, "issues")), (2, 5));
        let seen = t.seen.borrow();
        assert!(seen[1].1.contains("&nextPageToken=CAEaAggD"), "{}", seen[1].1);
        assert_eq!(g.value.get("isLast").and_then(Value::as_bool), Some(true));
        // The limit stops it early.
        let t = Script::new(vec![Ok(ok(&fixture("jira-search-1.json"))), Ok(ok(&fixture("jira-search-2.json")))]);
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        assert_eq!(count(&c.jql("project = PROJ", 3).unwrap().value, "issues"), 3);
        assert_eq!(t.seen.borrow().len(), 1);
    }

    #[test]
    fn cql_pagination_follows_the_next_link_on_the_configured_site() {
        let s = settings("https://cql.example.atlassian.net", true);
        let t = Script::new(vec![Ok(ok(&fixture("confluence-search-1.json"))), Ok(ok(&fixture("confluence-search-2.json")))]);
        let sl = |_: Duration| {};
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        let g = c.cql("type = page", 50).unwrap();
        assert_eq!((g.pages, count(&g.value, "results")), (2, 3));
        let seen = t.seen.borrow();
        assert!(seen[1].1.starts_with("https://cql.example.atlassian.net/wiki/rest/api/search?"), "{}", seen[1].1);
        assert!(seen[1].1.contains("cursor="), "{}", seen[1].1);
        // A next link off the search path is not followed.
        let mut evil = json::parse(&fixture("confluence-search-1.json")).unwrap();
        if let Some(Value::Object(l)) = evil.as_object_mut().and_then(|m| m.get_mut("_links")) {
            l.insert("next".into(), Value::String("https://elsewhere.example/steal".into()));
        }
        let t = Script::new(vec![Ok(ok(&json::to_string(&evil)))]);
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        assert_eq!(c.cql("type = page", 50).unwrap().pages, 1);
    }

    #[test]
    fn a_redirect_to_another_origin_drops_the_authorization_header() {
        let s = settings("https://redir.example.atlassian.net", true);
        let t = Script::new(vec![
            Ok(status(302, &[("location", "/rest/api/3/myself?again=1")])),
            Ok(status(302, &[("location", "https://cdn.example.net/x")])),
            Ok(status(302, &[("location", "https://redir.example.atlassian.net/back")])),
            Ok(ok("{}")),
        ]);
        let sl = |_: Duration| {};
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        c.myself().unwrap();
        let auths: Vec<bool> = t.seen.borrow().iter().map(|x| x.2).collect();
        assert_eq!(auths, vec![true, true, false, false], "kept on the same origin, dropped after leaving it, and not restored");
        assert!(t.seen.borrow()[2].3.lines().all(|l| !l.contains("Authorization")));
        // A downgrade to plain http is refused.
        let t = Script::new(vec![Ok(status(301, &[("location", "http://redir.example.atlassian.net/x")]))]);
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        assert!(c.myself().unwrap_err().what.contains("does not follow"));
    }

    #[test]
    fn debug_logging_redacts_the_authorization_header() {
        let s = settings("https://debug.example.atlassian.net", false);
        let t = Script::new(vec![]);
        let sl = |_: Duration| {};
        let mut c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        c.debug = true;
        c.myself().unwrap();
        let log = c.debug_log.borrow().join("\n");
        assert!(
            log.contains("> GET https://debug.example.atlassian.net/rest/api/2/myself") && log.contains("> Authorization: [redacted]"),
            "{log}"
        );
        assert!(!log.contains("SENTINEL") && !log.contains("Bearer"), "{log}");
    }

    #[test]
    fn check_reports_presence_auth_and_write_capability() {
        let s = settings("https://check.example.atlassian.net", true);
        let perms = r#"{"permissions":{"CREATE_ISSUES":{"havePermission":false},"EDIT_ISSUES":{"havePermission":true}}}"#;
        let t = Script::new(vec![Ok(ok(&fixture("jira-myself.json"))), Ok(ok(perms)), Ok(ok(&fixture("confluence-user.json")))]);
        let ctx =
            Ctx { settings: Some(&s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &env };
        let mut out = Vec::new();
        check_with(&s, &ctx, &mut out, &t).unwrap();
        let o = String::from_utf8(out).unwrap();
        assert!(o.contains("site is an Atlassian Cloud host (*.atlassian.net)") && !o.contains("check.example"), "{o}");
        assert!(o.contains("env E: present\nenv T: present"), "{o}");
        assert!(o.contains("jira auth: ok (GET /rest/api/3/myself 200"), "{o}");
        assert!(o.contains("CREATE_ISSUES no, EDIT_ISSUES yes") && o.contains("writes WOULD be allowed"), "{o}");
        assert!(o.contains("confluence writes: WOULD be allowed for this account: create page"), "{o}");
        assert!(!o.contains("Avery") && !o.contains("e@example.com") && !o.contains("SENTINEL"), "no account data: {o}");
        // A variable absent: no request.
        let none = |n: &str| if n == "E" { Some("x".to_string()) } else { None };
        let ctx = Ctx { env: &none, ..ctx };
        let t = Script::new(vec![]);
        let mut out = Vec::new();
        check_with(&s, &ctx, &mut out, &t).unwrap();
        let o = String::from_utf8(out).unwrap();
        assert!(o.contains("env T: absent") && o.contains("not tried") && t.seen.borrow().is_empty(), "{o}");
    }

    #[test]
    fn smoke_prints_shapes_and_sizes_never_content() {
        let s = settings("https://smoke.example.atlassian.net", true);
        let t = Script::new(vec![
            Ok(ok(&fixture("jira-issue.json"))),
            Ok(ok(&fixture("jira-search-1.json"))),
            Ok(ok(&fixture("jira-search-2.json"))),
            Ok(ok(&fixture("confluence-page.json"))),
            Ok(ok(&fixture("confluence-search-1.json"))),
            Ok(ok(&fixture("confluence-search-2.json"))),
        ]);
        let ctx =
            Ctx { settings: Some(&s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &env };
        let args: Vec<String> = ["--issue", "PROJ-123", "--jql", "project = PROJ", "--page", "98765", "--cql", "type = page"]
            .iter()
            .map(|x| x.to_string())
            .collect();
        let mut out = Vec::new();
        smoke_with(&s, &ctx, &mut out, &args, &t).unwrap();
        let o = String::from_utf8(out).unwrap();
        assert!(
            o.contains("issue: GET /rest/api/3/issue/{key} 1 page(s)") && o.contains("description adf, comments 3, links 1, attachments 2"),
            "{o}"
        );
        assert!(o.contains("jql: GET /rest/api/3/search/jql 2 page(s)") && o.contains("issues 5"), "{o}");
        assert!(o.contains("page: GET /wiki/api/v2/pages/{id} 1 page(s)") && o.contains("headings "), "{o}");
        assert!(o.contains("cql: GET /wiki/rest/api/search 2 page(s)") && o.contains("results 3"), "{o}");
        // Content never appears: summaries, names, titles, keys, bodies.
        for word in [
            "Checkout fails",
            "Avery",
            "PROJ-123",
            "PROJ-201",
            "Release checklist",
            "freeze",
            "Export the report",
            "98765",
            "smoke.example",
        ] {
            assert!(!o.contains(word), "{word} leaked into smoke output:\n{o}");
        }
    }

    /// Every file under `dir`, its bytes as lossy text.
    fn tree_text(dir: &std::path::Path) -> String {
        let mut s = String::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                s.push_str(&tree_text(&p));
            } else {
                s.push_str(&String::from_utf8_lossy(&std::fs::read(&p).unwrap()));
            }
        }
        s
    }

    #[test]
    fn a_body_echoing_the_credential_is_scrubbed_in_the_handle_and_stdout() {
        let s = settings("https://echo.example.atlassian.net", true);
        let b64 = crate::trackers::base64(b"e@example.com:tok-SENTINEL-value");
        let tok64 = crate::trackers::base64(b"tok-SENTINEL-value");
        let body = format!(
            r#"{{"key":"PROJ-9","fields":{{"summary":"echo"}},"headers":{{"Authorization":"Basic {b64}"}},"who":"e@example.com","t":"tok-SENTINEL-value","t64":"{tok64}"}}"#
        );
        // The auth material only: the account email is not a secret
        // (journal:ATLAS-TRACKERS:0015) and stays in the handle.
        let forms = ["tok-SENTINEL-value", b64.as_str(), tok64.as_str()];
        let dir = std::env::temp_dir().join(format!("atlas-trackers-echo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("atlas.db");
        let ctx = Ctx { settings: Some(&s), db: &db, lean: None, store: &dir, env: &env };
        for word in ["issue", "smoke"] {
            let t = Script::new(vec![Ok(ok(&body))]);
            let mut out = Vec::new();
            if word == "issue" {
                read_with(&s, &ctx, &mut out, "issue", "PROJ-9", &ReadOpts::default(), &t, crate::handles::HintForm::Recall).unwrap();
            } else {
                smoke_with(&s, &ctx, &mut out, &["--issue".to_string(), "PROJ-9".to_string()], &t).unwrap();
            }
            let o = String::from_utf8(out).unwrap();
            for f in forms {
                assert!(!o.contains(f), "{word}: {f} in stdout:\n{o}");
            }
        }
        let files = tree_text(&dir);
        for f in forms {
            assert!(!files.contains(f), "{f} in the handle files");
        }
        assert!(files.contains("[redacted]"), "the handle was written and scrubbed");
        assert!(files.contains("\"who\": \"e@example.com\""), "the account email passes through as a field value");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confluence_counts_only_known_write_verbs() {
        let s = settings("https://verbs.example.atlassian.net", true);
        let perms = r#"{"permissions":{"CREATE_ISSUES":{"havePermission":false}}}"#;
        let user =
            r#"{"accountId":"x","operations":[{"operation":"read","targetType":"page"},{"operation":"use","targetType":"application"}]}"#;
        let t = Script::new(vec![Ok(ok(&fixture("jira-myself.json"))), Ok(ok(perms)), Ok(ok(user))]);
        let ctx =
            Ctx { settings: Some(&s), db: std::path::Path::new("unused"), lean: None, store: std::path::Path::new("unused"), env: &env };
        let mut out = Vec::new();
        check_with(&s, &ctx, &mut out, &t).unwrap();
        let o = String::from_utf8(out).unwrap();
        assert!(o.contains("confluence writes: reads only"), "{o}");
        assert!(o.contains("confluence other operations (not counted as writes): use application"), "{o}");
    }

    #[test]
    fn an_https_site_never_follows_a_redirect_to_http_loopback() {
        let s = settings("https://down.example.atlassian.net", true);
        let t = Script::new(vec![Ok(status(302, &[("location", "http://127.0.0.1:8080/x")])), Ok(ok("{}"))]);
        let sl = |_: Duration| {};
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        assert!(c.myself().unwrap_err().what.contains("does not follow"));
        assert_eq!(t.seen.borrow().len(), 1);
    }

    #[test]
    fn http_400_names_the_query() {
        let s = settings("https://bad.example.atlassian.net", true);
        let t = Script::new(vec![Ok(status(400, &[]))]);
        let sl = |_: Duration| {};
        let c = Client::new(&s, Credential::resolve(&s.auth, &env).unwrap(), &t, &sl);
        let e = c.jql("order by created", 5).unwrap_err().to_string();
        assert!(e.contains("HTTP 400 (bad request: check the JQL or CQL"), "{e}");
    }
}
