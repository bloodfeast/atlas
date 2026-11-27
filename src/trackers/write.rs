//! Project tracking writes: a Jira comment, a transition, a field edit
//! (assignee, labels, sprint) and a Confluence page update.
//!
//! A module of its own (journal:ATLAS-TRACKERS:0044): the read [`super::api::Client`]
//! stays GET-only, and every call a write makes is in [`WRITE_ENDPOINTS`] or
//! [`PRE_READS`]. Each verb is one function shared by the CLI and the MCP op
//! (`note op=track`); both are dry-run unless `apply` is set, and both go
//! through the allow-list (`writes.jira.PROJ`, `writes.confluence.SPACE`).
//! The write token comes from variables of its own and is resolved only on
//! `--apply`; every pre-read (current status, transitions, user, field list,
//! page version) uses the read token. One line comes back, and an applied
//! write is journaled once.

use std::process::ExitCode;
use std::time::Duration;

use super::api::{enc, origin, Client, Endpoint, Product};
use super::http::{Failure, Method, Request, Response, Transport, WriteBody, WriteMethod};
use super::{Credential, Ctx, HostClass, Settings};
use crate::json;
use crate::json::{Map, Value};
use crate::memory::{Draft, MemoryStore};

/// The four writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verb {
    /// A Jira comment.
    #[default]
    Comment,
    /// A Jira transition.
    Transition,
    /// A Jira field edit (assignee, labels, sprint).
    Edit,
    /// A Confluence page update.
    PageUpdate,
}

impl Verb {
    /// The command word.
    pub fn word(self) -> &'static str {
        match self {
            Verb::Comment => "comment",
            Verb::Transition => "transition",
            Verb::Edit => "edit",
            Verb::PageUpdate => "page-update",
        }
    }

    /// The verb of a command word or an op argument (`page-update` or `page_update`).
    pub fn parse(s: &str) -> Option<Verb> {
        [Verb::Comment, Verb::Transition, Verb::Edit, Verb::PageUpdate].into_iter().find(|v| v.word() == s.replace('_', "-"))
    }

    fn confluence(self) -> bool {
        self == Verb::PageUpdate
    }
}

/// One write's arguments: what the CLI flags and the op's fields both fill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteArgs {
    /// Which write.
    pub verb: Verb,
    /// The issue key, or the page id.
    pub target: String,
    /// A comment's text, or a page's body (section or whole).
    pub body: Option<String>,
    /// Send a comment although the same one went within ten minutes.
    pub again: bool,
    /// A transition's target: a status or transition name, or a transition id.
    pub to: Option<String>,
    /// An edit's assignee: `me`, `none`, an account id or a name to look up.
    pub assignee: Option<String>,
    /// Labels to add.
    pub labels_add: Vec<String>,
    /// Labels to remove.
    pub labels_remove: Vec<String>,
    /// A sprint's numeric id, or `none`.
    pub sprint: Option<String>,
    /// A page update's heading: replace (or with `append`, extend) that section.
    pub section: Option<String>,
    /// Append the body to the page, or to the named section.
    pub append: bool,
    /// A page's new title.
    pub title: Option<String>,
    /// Refuse unless the page is at this version.
    pub expect_version: Option<u64>,
    /// A page version's message.
    pub message: Option<String>,
    /// Send it (without: show the request and send nothing).
    pub apply: bool,
    /// The journal lane.
    pub lane: Option<String>,
}

/// What a write answers: the text, and the exit code (0 done or shown, 1
/// failed, 2 refused with nothing sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The text, one line when it succeeded.
    pub text: String,
    /// The exit code.
    pub code: u8,
}

/// What a write runs with besides the configuration: the transport, the
/// sleep and the clock (seconds), so the tests script all three.
pub struct Env<'a> {
    /// Sends requests.
    pub t: &'a dyn Transport,
    /// Waits.
    pub sleep: &'a dyn Fn(Duration),
    /// Seconds since the epoch.
    pub now_s: u64,
}

/// A write call atlas may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteEndpoint {
    /// Its name.
    pub name: &'static str,
    /// `POST` or `PUT`.
    pub method: WriteMethod,
    /// Its product.
    pub product: Product,
    /// Its path on Cloud (`{key}` or `{id}` filled in).
    pub cloud: &'static str,
    /// Its path on Data Center and Server.
    pub dc: &'static str,
}

/// Every call a write sends. Nothing deletes.
pub const WRITE_ENDPOINTS: &[WriteEndpoint] = &[
    WriteEndpoint {
        name: "comment",
        method: WriteMethod::Post,
        product: Product::Jira,
        cloud: "/rest/api/3/issue/{key}/comment",
        dc: "/rest/api/2/issue/{key}/comment",
    },
    WriteEndpoint {
        name: "transition",
        method: WriteMethod::Post,
        product: Product::Jira,
        cloud: "/rest/api/3/issue/{key}/transitions",
        dc: "/rest/api/2/issue/{key}/transitions",
    },
    WriteEndpoint {
        name: "edit",
        method: WriteMethod::Put,
        product: Product::Jira,
        cloud: "/rest/api/3/issue/{key}",
        dc: "/rest/api/2/issue/{key}",
    },
    WriteEndpoint {
        name: "page_update",
        method: WriteMethod::Put,
        product: Product::Confluence,
        cloud: "/api/v2/pages/{id}",
        dc: "/rest/api/content/{id}",
    },
];

const fn pre(name: &'static str, product: Product, cloud: &'static str, dc: &'static str) -> Endpoint {
    Endpoint { name, method: Method::Get, product, cloud, dc }
}

/// The reads a write makes first, with the read token (GET only).
pub const PRE_READS: &[Endpoint] = &[
    pre("issue_state", Product::Jira, "/rest/api/3/issue/{key}", "/rest/api/2/issue/{key}"),
    pre("transitions", Product::Jira, "/rest/api/3/issue/{key}/transitions", "/rest/api/2/issue/{key}/transitions"),
    pre("assignable", Product::Jira, "/rest/api/3/user/assignable/search", "/rest/api/2/user/assignable/search"),
    pre("fields", Product::Jira, "/rest/api/3/field", "/rest/api/2/field"),
    pre("myself", Product::Jira, "/rest/api/3/myself", "/rest/api/2/myself"),
    pre("space", Product::Confluence, "/api/v2/spaces/{id}", "/rest/api/space/{id}"),
];

fn is_flag(a: &str) -> bool {
    a.starts_with("--")
}

fn list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

impl WriteArgs {
    /// The arguments of `atlas track VERB ...` (the target, then flags). A
    /// `--body-file` is read now.
    ///
    /// # Errors
    ///
    /// A message for the user: a missing target, an unknown flag, a flag
    /// without its value, a file that cannot be read.
    pub fn from_argv(verb: Verb, args: &[String]) -> Result<WriteArgs, String> {
        let cmd = verb.word();
        let target = args.first().filter(|a| !is_flag(a)).ok_or_else(|| format!("atlas track {cmd}: missing its {}", if verb.confluence() { "page id" } else { "issue key" }))?;
        let mut a = WriteArgs { verb, target: target.clone(), ..WriteArgs::default() };
        let mut i = 1;
        while i < args.len() {
            let flag = args[i].as_str();
            match flag {
                "--again" => a.again = true,
                "--apply" => a.apply = true,
                "--append" => a.append = true,
                "--body" | "--body-file" | "--to" | "--assignee" | "--labels-add" | "--labels-remove" | "--sprint" | "--section" | "--title"
                | "--expect-version" | "--message" | "--lane" => {
                    i += 1;
                    let v = args.get(i).ok_or_else(|| format!("atlas track {cmd}: {flag} needs a value"))?.clone();
                    match flag {
                        "--body" => a.body = Some(v),
                        "--body-file" => {
                            a.body = Some(std::fs::read_to_string(&v).map_err(|e| format!("atlas track {cmd}: --body-file {v}: {e}"))?)
                        }
                        "--to" => a.to = Some(v),
                        "--assignee" => a.assignee = Some(v),
                        "--labels-add" => a.labels_add = list(&v),
                        "--labels-remove" => a.labels_remove = list(&v),
                        "--sprint" => a.sprint = Some(v),
                        "--section" => a.section = Some(v),
                        "--title" => a.title = Some(v),
                        "--expect-version" => {
                            a.expect_version = Some(v.parse().map_err(|_| format!("atlas track {cmd}: --expect-version {v:?}: a number"))?)
                        }
                        "--message" => a.message = Some(v),
                        _ => a.lane = Some(v),
                    }
                }
                _ => return Err(format!("atlas track {cmd}: unknown argument {flag:?}\n{}", super::USAGE)),
            }
            i += 1;
        }
        Ok(a)
    }

    /// The arguments of the `note op=track` call: the same fields, named
    /// `verb`, `target`, `body`, `body_file`, `again`, `to`, `assignee`,
    /// `labels_add`, `labels_remove` (a comma list or an array), `sprint`,
    /// `section`, `append`, `title`, `expect_version`, `message`, `apply`
    /// and `lane`.
    ///
    /// # Errors
    ///
    /// A message for the user.
    pub fn from_map(m: &Map) -> Result<WriteArgs, String> {
        let st = |k: &str| -> Result<Option<String>, String> {
            match m.get(k) {
                None | Some(crate::json::Value::Null) => Ok(None),
                Some(crate::json::Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => Err(format!("track: {k} must be a string")),
            }
        };
        let bl = |k: &str| -> Result<bool, String> {
            match m.get(k) {
                None | Some(crate::json::Value::Null) => Ok(false),
                Some(crate::json::Value::Bool(b)) => Ok(*b),
                Some(_) => Err(format!("track: {k} must be a boolean")),
            }
        };
        let ls = |k: &str| -> Result<Vec<String>, String> {
            match m.get(k) {
                None | Some(crate::json::Value::Null) => Ok(vec![]),
                Some(crate::json::Value::String(s)) => Ok(list(s)),
                Some(crate::json::Value::Array(v)) => {
                    v.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| format!("track: {k} holds strings"))).collect()
                }
                Some(_) => Err(format!("track: {k} must be a comma list or an array")),
            }
        };
        let verb = st("verb")?
            .as_deref()
            .and_then(Verb::parse)
            .ok_or("track: verb is one of comment, transition, edit, page_update")?;
        let target = st("target")?.ok_or("track: target is required (an issue key, or a page id)")?;
        let body = match (st("body")?, st("body_file")?) {
            (Some(b), None) => Some(b),
            (None, Some(f)) => Some(std::fs::read_to_string(&f).map_err(|e| format!("track: body_file {f}: {e}"))?),
            (None, None) => None,
            (Some(_), Some(_)) => return Err("track: give body or body_file, not both".into()),
        };
        let expect_version = match m.get("expect_version") {
            None | Some(crate::json::Value::Null) => None,
            Some(v) => Some(v.as_u64().ok_or("track: expect_version must be a number")?),
        };
        Ok(WriteArgs {
            verb,
            target,
            body,
            again: bl("again")?,
            to: st("to")?,
            assignee: st("assignee")?,
            labels_add: ls("labels_add")?,
            labels_remove: ls("labels_remove")?,
            sprint: st("sprint")?,
            section: st("section")?,
            append: bl("append")?,
            title: st("title")?,
            expect_version,
            message: st("message")?,
            apply: bl("apply")?,
            lane: st("lane")?,
        })
    }
}

/// `atlas track comment|transition|edit|page-update ...`.
///
/// # Errors
///
/// A message for the user (bad arguments); a refusal or failure of the write
/// itself is the exit code.
pub fn cli(word: &str, args: &[String], ctx: &Ctx<'_>, s: &Settings) -> Result<ExitCode, String> {
    let verb = Verb::parse(word).ok_or_else(|| format!("atlas track: unknown command {word:?}"))?;
    let a = WriteArgs::from_argv(verb, args)?;
    let curl = super::http::Curl::default();
    let sleep = |d: Duration| std::thread::sleep(d);
    let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let r = run(ctx, s, &Env { t: &curl, sleep: &sleep, now_s }, &a);
    // What was done or shown goes to stdout; a refusal or failure to stderr.
    if r.code == 0 {
        println!("{}", r.text);
    } else {
        eprintln!("{}", r.text);
    }
    Ok(ExitCode::from(r.code))
}

/// The `note op=track` call: the same function as the command.
///
/// # Errors
///
/// A message for the user (bad arguments).
pub fn run_map(ctx: &Ctx<'_>, s: &Settings, m: &Map) -> Result<Reply, String> {
    let a = WriteArgs::from_map(m)?;
    let curl = super::http::Curl::default();
    let sleep = |d: Duration| std::thread::sleep(d);
    let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    Ok(run(ctx, s, &Env { t: &curl, sleep: &sleep, now_s }, &a))
}

/// What replacing or extending one section did to a page body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionEdit {
    /// The whole new body.
    pub body: String,
    /// The section's content size before, in bytes.
    pub old_len: usize,
    /// The section's content size after, in bytes.
    pub new_len: usize,
}

/// `body` with the section under `heading` replaced by `new` (or `new` added
/// at its end when `append`).
///
/// A section is scoped to the heading's container: it ends at the next
/// heading of the same or a higher level in that same element, or at the
/// element's closing tag (a layout cell, a table cell, a macro body), so
/// neither a replace nor an append crosses into the next column or cell.
/// A heading nested deeper inside the section is part of it. Headings in
/// CDATA (a code macro) and comments are not headings.
///
/// # Errors
///
/// A message naming how many headings matched when it is not one, or why
/// the section cannot be scoped: the body or `new` does not nest.
pub fn section_edit(body: &str, heading: &str, new: &str, append: bool) -> Result<SectionEdit, String> {
    let hs = headings(body).map_err(|m| format!("the page body does not nest ({m}), so a section cannot be scoped; replace the whole page"))?;
    tags(new).and_then(|t| nesting(new, &t)).map_err(|m| format!("the new section does not nest on its own ({m}); it would cross its container"))?;
    let want = heading.trim();
    let found: Vec<&Heading> = hs.iter().filter(|h| h.text == want).collect();
    let [h] = found.as_slice() else {
        return Err(format!("{} headings named {want:?} (a section needs exactly one)", found.len()));
    };
    let end = hs.iter().find(|n| n.start >= h.end && n.start < h.scope_end && n.depth == h.depth && n.level <= h.level).map_or(h.scope_end, |n| n.start);
    let old_len = end - h.end;
    Ok(if append {
        SectionEdit { body: format!("{}{new}{}", &body[..end], &body[end..]), old_len, new_len: old_len + new.len() }
    } else {
        SectionEdit { body: format!("{}{new}{}", &body[..h.end], &body[end..]), old_len, new_len: new.len() }
    })
}

/// One heading element of a storage body.
struct Heading {
    /// Where its opening tag starts.
    start: usize,
    /// Where its closing tag ends: the section's content starts here.
    end: usize,
    level: u8,
    /// Its text without markup, entities decoded.
    text: String,
    /// How many elements enclose it.
    depth: usize,
    /// Where its container's closing tag starts (the body's end at the top
    /// level): no section of it runs past here.
    scope_end: usize,
}

/// One tag of a storage body.
struct Tag<'b> {
    start: usize,
    /// Just past its `>`.
    end: usize,
    name: &'b str,
    close: bool,
}

/// Elements that never take a closing tag, even written without `/>`.
const VOID: &[&str] = &["br", "hr", "img", "col", "input", "meta", "link", "area", "base", "wbr", "source", "param", "embed", "track"];

/// The opening and closing tags of `body`, in order; self-closed and void
/// elements, CDATA sections, comments and declarations are left out.
fn tags(body: &str) -> Result<Vec<Tag<'_>>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = body[i..].find('<') {
        let s = i + p;
        let rest = &body[s..];
        if let Some((o, c)) = [("<![CDATA[", "]]>"), ("<!--", "-->")].into_iter().find(|(o, _)| rest.starts_with(o)) {
            let e = rest[o.len()..].find(c).ok_or_else(|| format!("{o} without its {c}"))?;
            i = s + o.len() + e + c.len();
            continue;
        }
        let e = rest.find('>').ok_or_else(|| format!("a tag at byte {s} without its >"))?;
        let inner = &rest[1..e];
        i = s + e + 1;
        if inner.starts_with('!') || inner.starts_with('?') {
            continue;
        }
        let close = inner.starts_with('/');
        let name = inner.trim_start_matches('/').split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        if name.is_empty() {
            return Err(format!("a bare < at byte {s}"));
        }
        if close || !(inner.ends_with('/') || VOID.contains(&name)) {
            out.push(Tag { start: s, end: i, name, close });
        }
    }
    Ok(out)
}

/// The level of a heading element's name (`h1` to `h6`).
fn heading_level(name: &str) -> Option<u8> {
    match name.as_bytes() {
        [b'h', d @ b'1'..=b'6'] => Some(d - b'0'),
        _ => None,
    }
}

/// The headings of `body` from its tags, each with its depth and the end of
/// its container, in order; refuses tags that do not nest.
fn nesting(body: &str, tags: &[Tag<'_>]) -> Result<Vec<Heading>, String> {
    // The open elements: name, open tag start and end, the headings directly inside.
    let mut open: Vec<(&str, usize, usize, Vec<usize>)> = Vec::new();
    let mut out: Vec<Heading> = Vec::new();
    for t in tags {
        if !t.close {
            open.push((t.name, t.start, t.end, Vec::new()));
            continue;
        }
        let Some((name, start, open_end, kids)) = open.pop() else {
            return Err(format!("a stray </{}> at byte {}", t.name, t.start));
        };
        if name != t.name {
            return Err(format!("<{name}> closed by </{}> at byte {}", t.name, t.start));
        }
        for k in kids {
            out[k].scope_end = t.start;
        }
        if let Some(level) = heading_level(name) {
            out.push(Heading { start, end: t.end, level, text: plain(&body[open_end..t.start]), depth: open.len(), scope_end: body.len() });
            if let Some(parent) = open.last_mut() {
                parent.3.push(out.len() - 1);
            }
        }
    }
    if let Some((name, start, ..)) = open.last() {
        return Err(format!("<{name}> at byte {start} is never closed"));
    }
    out.sort_by_key(|h| h.start);
    Ok(out)
}

/// The heading elements `<h1>` to `<h6>` of `body`, in order.
fn headings(body: &str) -> Result<Vec<Heading>, String> {
    nesting(body, &tags(body)?)
}

/// `inner` without tags, with the common entities decoded, trimmed.
fn plain(inner: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in inner.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text.replace("&nbsp;", " ").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&#39;", "'");
    text.replace("&amp;", "&").trim().to_string()
}

/// The `check` line for one product's writes: its token variable (set or
/// not, never its value) and how many verbs the allow-list names.
pub fn check_line(s: &Settings, ctx: &Ctx<'_>, confluence: bool) -> String {
    let product = if confluence { "confluence" } else { "jira" };
    match s.write_auth(confluence) {
        Err(key) => format!("{product} write token: not configured ({key}); writes refused"),
        Ok(auth) => {
            let var = auth.env_names().last().copied().unwrap_or("");
            let set = (ctx.env)(var).is_some_and(|v| !v.is_empty());
            let list = if confluence { &s.write.confluence } else { &s.write.jira };
            let verbs: usize = list.iter().map(|(_, v)| v.len()).sum();
            let (unit, n) = (if confluence { "space" } else { "project" }, list.len());
            format!(
                "{product} write token: {var} {}; allowed verbs {verbs} ({n} {unit}{})",
                if set { "set" } else { "unset" },
                if n == 1 { "" } else { "s" }
            )
        }
    }
}

fn refuse(t: impl Into<String>) -> Reply {
    Reply { text: t.into(), code: 2 }
}

fn fail(t: impl Into<String>) -> Reply {
    Reply { text: t.into(), code: 1 }
}

fn done(t: impl Into<String>) -> Reply {
    Reply { text: t.into(), code: 0 }
}

type Res<T> = Result<T, Reply>;

/// `s` cut to at most `max` bytes at a character boundary.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].to_string()
}

/// The longest success reply, in bytes.
const REPLY_MAX: usize = 200;
/// The longest dry-run reply, in bytes.
const DRY_MAX: usize = 1200;
/// A comment sent again within this many seconds needs `--again`.
const DUPLICATE_WINDOW_S: u64 = 600;
/// The most of a body a dry-run prints, in bytes.
const BODY_SHOWN: usize = 300;

fn eq_ic(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn text_of(v: &Value, ptr: &str) -> String {
    match v.pointer(ptr) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// A Jira comment body in Atlassian Document Format: one paragraph per blank
/// line, a hard break per line break; plain text, no Markdown.
fn adf(text: &str) -> Value {
    let paras: Vec<Value> = text
        .split("\n\n")
        .map(|p| p.trim_matches(|c| c == '\n' || c == '\r'))
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut content: Vec<Value> = Vec::new();
            for (i, line) in p.split('\n').enumerate() {
                if i > 0 {
                    content.push(json!({"type": "hardBreak"}));
                }
                let line = line.trim_end_matches('\r');
                if !line.is_empty() {
                    content.push(json!({"type": "text", "text": line}));
                }
            }
            json!({"type": "paragraph", "content": content})
        })
        .collect();
    json!({"type": "doc", "version": 1, "content": paras})
}

/// One request a write is about to make.
struct Plan {
    method: WriteMethod,
    url: String,
    body: String,
    /// The endpoint as errors name it.
    shown: String,
}

/// What came back from a write.
enum Sent {
    Resp(Response),
    Down(Failure),
}

/// How an applied write ended, for the journal and the reply.
struct Outcome {
    /// The reply's first word: the issue key, or `page ID`.
    subject: String,
    /// What was done, one line.
    result: String,
    /// The HTTP status, or why there is none.
    status: String,
    /// The body that was sent.
    plan: Plan,
}

struct Cx<'a> {
    ctx: &'a Ctx<'a>,
    s: &'a Settings,
    env: &'a Env<'a>,
    a: &'a WriteArgs,
    /// The read client; `None` when the read variables are unset.
    c: Result<Client<'a>, String>,
    who: String,
    wvar: String,
    wcred: Option<Credential>,
    lane: String,
    /// A Jira write's issue, read first (`fields=project,status`): the
    /// allow-list is checked against its real project. Null for Confluence.
    issue: Value,
}

/// One write: refuse, show, or send.
pub fn run(ctx: &Ctx<'_>, s: &Settings, env: &Env<'_>, a: &WriteArgs) -> Reply {
    match go(ctx, s, env, a) {
        Ok(r) | Err(r) => r,
    }
}

fn valid_target(a: &WriteArgs) -> bool {
    if a.verb.confluence() {
        !a.target.is_empty() && a.target.len() <= 32 && a.target.bytes().all(|b| b.is_ascii_digit())
    } else {
        a.target.rsplit_once('-').is_some_and(|(p, n)| !p.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            && a.target.len() <= 64
            && a.target.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }
}

fn go<'a>(ctx: &'a Ctx<'a>, s: &'a Settings, env: &'a Env<'a>, a: &'a WriteArgs) -> Res<Reply> {
    let label = format!("atlas track {}", a.verb.word());
    let who = format!("{label} {}", a.target);
    let wauth = s
        .write_auth(a.verb.confluence())
        .map_err(|key| refuse(format!("{label}: no {key} in project_tracking.atlassian: this setup is read-only; nothing sent")))?;
    if !valid_target(a) {
        let what = if a.verb.confluence() { "a page id is digits" } else { "an issue key is PROJECT-NUMBER" };
        return Err(refuse(format!("{who}: {what}; nothing sent")));
    }
    if let Some(l) = &a.lane {
        if !super::key_ok(l) {
            return Err(refuse(format!("{who}: --lane is a lane name (letters, digits, - and _); nothing sent")));
        }
    }
    let read = Credential::resolve(&s.auth, ctx.env).map(|cred| {
        let mut c = Client::new(s, cred, env.t, env.sleep);
        if let Some(x) = &s.confluence_auth {
            c = c.with_confluence_env(x, ctx.env);
        }
        c
    });
    let mut cx = Cx {
        ctx,
        s,
        env,
        a,
        c: read.map_err(|m| m.to_string()),
        who,
        wvar: wauth.env_names().last().copied().unwrap_or("").to_string(),
        wcred: None,
        lane: s.write_lane(a.lane.as_deref()),
        issue: Value::Null,
    };
    let need = if a.verb.confluence() { Vec::new() } else { cx.jira_needs()? };
    cx.check_input()?;
    if a.apply {
        cx.wcred = Some(Credential::resolve(&wauth, ctx.env).map_err(|m| refuse(format!("{}: {m}; nothing sent", cx.who)))?);
        MemoryStore::open(ctx.store).map_err(|e| refuse(format!("{}: the journal store cannot be opened ({e}); nothing sent", cx.who)))?;
    }
    if !a.verb.confluence() {
        cx.allow_jira(&need)?;
    }
    match a.verb {
        Verb::Comment => cx.comment(),
        Verb::Transition => cx.transition(),
        Verb::Edit => cx.edit(),
        Verb::PageUpdate => cx.page_update(),
    }
}

/// The reads a write makes first, and the other GETs of this module.
fn pre_read(name: &str) -> &'static Endpoint {
    PRE_READS.iter().find(|e| e.name == name).expect("a listed pre-read")
}

fn write_ep(name: &str) -> &'static WriteEndpoint {
    WRITE_ENDPOINTS.iter().find(|e| e.name == name).expect("a listed write")
}

impl<'a> Cx<'a> {
    fn cloud(&self) -> bool {
        self.s.auth.is_cloud()
    }

    fn scrub(&self, t: &str) -> String {
        let t = self.c.as_ref().map_or_else(|_| t.to_string(), |c| c.scrub(t));
        self.wcred.as_ref().map_or_else(|| t.clone(), |w| w.scrub(&t))
    }

    fn client(&self) -> Res<&Client<'a>> {
        self.c.as_ref().map_err(|m| refuse(format!("{}: {m}; nothing sent", self.who)))
    }

    /// A read with the read token.
    fn read(&self, name: &str, base: &str, path_q: &str) -> Res<Value> {
        let e = pre_read(name);
        let c = self.client()?;
        c.get(e, base, path_q).map(|g| g.value).map_err(|e| fail(format!("{}: {e}", self.who)))
    }

    fn jira_path(&self, name: &str) -> String {
        let e = pre_read(name);
        let p = if self.cloud() { e.cloud } else { e.dc };
        format!("{}{}", self.s.jira_base(), p.replace("{key}", &enc(&self.a.target)))
    }

    fn conf_base(&self) -> Res<String> {
        self.s.confluence_base().map_err(|m| refuse(format!("{}: {m}; nothing sent", self.who)))
    }

    /// The allow-list words a Jira write needs; refuses an edit that changes
    /// nothing, before any request.
    fn jira_needs(&self) -> Res<Vec<&'static str>> {
        let a = self.a;
        let mut need: Vec<&str> = Vec::new();
        match a.verb {
            Verb::Comment => need.push("comment"),
            Verb::Transition => need.push("transition"),
            _ => {
                if a.assignee.is_some() {
                    need.push("assignee");
                }
                if !a.labels_add.is_empty() || !a.labels_remove.is_empty() {
                    need.push("labels");
                }
                if a.sprint.is_some() {
                    need.push("sprint");
                }
                if need.is_empty() {
                    return Err(refuse(format!(
                        "{}: nothing to change: give --assignee, --labels-add, --labels-remove or --sprint; nothing sent",
                        self.who
                    )));
                }
            }
        }
        Ok(need)
    }

    /// Refuses a Jira write the allow-list does not name, first for the
    /// typed key's project (before any request), then for the issue's real
    /// project, read with one GET (`fields=project,status`, read token): a
    /// moved issue keeps answering to its old key, so the prefix alone is
    /// never trusted.
    fn allow_jira(&mut self, need: &[&str]) -> Res<()> {
        let a = self.a;
        let typed = a.target.rsplit_once('-').map_or("", |(p, _)| p);
        self.allowed(typed, need, "")?;
        let path = format!("{}?fields=project,status", self.jira_path("issue_state").trim_start_matches(self.s.jira_base()));
        self.issue = self.read("issue_state", self.s.jira_base(), &path)?;
        let project = text_of(&self.issue, "/fields/project/key");
        if project.is_empty() {
            return Err(refuse(format!("{}: the issue's project could not be read (fields.project.key); nothing sent", self.who)));
        }
        let moved = if project == typed { String::new() } else { format!("; {} is now in {project}", self.a.target) };
        self.allowed(&project, need, &moved)
    }

    /// Refuses unless the allow-list names every word of `need` for `project`.
    fn allowed(&self, project: &str, need: &[&str], moved: &str) -> Res<()> {
        let entry = self.s.write.jira.iter().find(|(p, _)| p == project);
        let missing: Vec<&str> = need.iter().copied().filter(|v| !entry.is_some_and(|(_, l)| l.iter().any(|x| x == v))).collect();
        if missing.is_empty() {
            return Ok(());
        }
        let why = match entry {
            None => format!("project_tracking.atlassian.writes.jira has no {project}"),
            Some((_, l)) if l.is_empty() => format!("project_tracking.atlassian.writes.jira.{project} allows: nothing"),
            Some((_, l)) => format!("project_tracking.atlassian.writes.jira.{project} allows: {}", l.join(", ")),
        };
        Err(refuse(format!(
            "{}: refused: {} {} not allowed for Jira project {project} ({why}{moved}); nothing sent",
            self.who,
            missing.join(", "),
            if missing.len() == 1 { "is" } else { "are" }
        )))
    }

    /// The arguments' own checks, before any request.
    fn check_input(&self) -> Res<()> {
        let a = self.a;
        let bad = |m: &str| Err(refuse(format!("{}: {m}; nothing sent", self.who)));
        match a.verb {
            Verb::Comment if a.body.as_deref().is_none_or(|b| b.trim().is_empty()) => bad("a comment needs --body or --body-file with text"),
            Verb::Transition if a.to.as_deref().is_none_or(|t| t.trim().is_empty()) => bad("a transition needs --to NAME|ID"),
            Verb::Edit => {
                if let Some(sp) = &a.sprint {
                    if sp != "none" && !(!sp.is_empty() && sp.len() <= 12 && sp.bytes().all(|b| b.is_ascii_digit())) {
                        return bad("--sprint takes a numeric sprint id or none (a sprint name is not looked up)");
                    }
                }
                if a.labels_add.iter().chain(&a.labels_remove).any(|l| l.contains(char::is_whitespace)) {
                    return bad("a label holds no spaces");
                }
                Ok(())
            }
            Verb::PageUpdate if a.body.is_none() => bad("a page update needs --body-file (or --body) with the new content"),
            _ => Ok(()),
        }
    }

    // ---- sending

    fn send(&self, p: &Plan) -> Sent {
        let cred = self.wcred.as_ref().expect("the write credential is resolved before a send");
        let mut tries = 0u32;
        loop {
            super::api::pace_site(self.s, &origin(&p.url), self.env.sleep);
            let req = Request {
                method: Method::Get,
                write: Some(WriteBody { method: p.method, body: &p.body }),
                url: &p.url,
                auth: Some(cred.header()),
                timeout: self.s.timeout(),
                max_bytes: 4 << 20,
            };
            match self.env.t.send(&req) {
                Ok(r) if r.status == 429 && tries < self.s.max_retries => {
                    let backoff = Duration::from_millis(self.s.backoff_ms.saturating_mul(1 << tries.min(16)));
                    let asked = r.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()).map(Duration::from_secs);
                    (self.env.sleep)(asked.unwrap_or(backoff).min(Duration::from_secs(self.s.max_wait_s)));
                    tries += 1;
                }
                Ok(r) => return Sent::Resp(r),
                Err(f) => return Sent::Down(f),
            }
        }
    }

    /// Waits one backoff step before a retry.
    fn backoff(&self, tries: u32) {
        (self.env.sleep)(Duration::from_millis(self.s.backoff_ms.saturating_mul(1 << tries.min(16))).min(Duration::from_secs(self.s.max_wait_s)));
    }

    /// The failure line for a response that was not a success.
    fn http_fail(&self, p: &Plan, r: &Response, base: &str) -> Reply {
        let st = r.status;
        let detail = self.details(&r.body);
        let why = match st {
            400 => format!("bad request: {detail}; a missing permission shows as 400 or 404"),
            401 => format!(
                "unauthorized: check the write credential's environment variables{}",
                if self.cloud() && HostClass::of(base) == HostClass::Cloud { "; a scoped token needs the api.atlassian.com gateway URL" } else { "" }
            ),
            403 => "forbidden: the account or token may not do this".to_string(),
            404 => "not found, or this account may not view or edit it".to_string(),
            409 => "conflicting update: the issue or page changed while it was being written".to_string(),
            413 => "a per-issue limit is reached (comments, attachments, worklogs or links)".to_string(),
            422 => "a configuration problem in the workflow or a validator".to_string(),
            429 => format!("rate limited; gave up after {} retries", self.s.max_retries),
            500..=599 => "server error".to_string(),
            300..=399 => "a redirect, which a write never follows".to_string(),
            _ => format!("unexpected status: {detail}"),
        };
        fail(self.scrub(&format!("{}: {}: HTTP {st} ({why})", self.who, p.shown)))
    }

    /// The error messages a response body carries, scrubbed and short.
    fn details(&self, body: &[u8]) -> String {
        let Ok(v) = json::parse(&String::from_utf8_lossy(body)) else { return "no details".into() };
        let mut parts: Vec<String> = Vec::new();
        for m in v.get("errorMessages").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            parts.push(m.to_string());
        }
        for (k, m) in v.get("errors").and_then(Value::as_object).into_iter().flat_map(Map::iter) {
            parts.push(format!("{k}: {}", m.as_str().unwrap_or("")));
        }
        for e in v.get("errors").and_then(Value::as_array).into_iter().flatten() {
            parts.push(format!("{} {}", text_of(e, "/title"), text_of(e, "/detail")).trim().to_string());
        }
        if parts.is_empty() {
            return "no details".into();
        }
        cut(&self.scrub(&parts.join("; ")), 300)
    }

    // ---- showing, and recording

    /// Stores `body` under a handle; its id, when the store works.
    fn handle(&self, what: &str, body: &str) -> Option<String> {
        let mut h = crate::handles::Handles::open(self.ctx.db, crate::handles::HandleCfg::from_lean(self.ctx.lean)).ok()?;
        let meta = crate::handles::Meta {
            kind: "track-write".into(),
            label: format!("{} {what}", self.who),
            cmd: String::new(),
            cwd: String::new(),
            session: String::new(),
            exit_code: None,
            ms: None,
        };
        let row = h.put(self.scrub(body).as_bytes(), &meta).ok()?;
        Some(format!("h:{}", row.id.trim_start_matches("h:")))
    }

    /// The dry-run reply: the request as it would go, nothing sent.
    fn dry(&self, p: &Plan, extra: &[String]) -> Reply {
        let handle = self.handle("dry-run", &p.body).map(|h| format!(", {h}")).unwrap_or_default();
        let set = (self.ctx.env)(&self.wvar).is_some_and(|v| !v.is_empty());
        let mut t = format!("{} {}\nAuthorization: [redacted]\nContent-Type: application/json\n", p.method.name(), cut(&p.url, 300));
        t.push_str(&format!("body ({} B{handle}): {}\n", p.body.len(), cut(&self.scrub(&p.body), BODY_SHOWN)));
        let project = text_of(&self.issue, "/fields/project/key");
        if !project.is_empty() {
            t.push_str(&format!("project: {project} (read from the issue)\n"));
        }
        for e in extra {
            t.push_str(&cut(e, 200));
            t.push('\n');
        }
        t.push_str(&format!("write variable {}: {}\nsend: add --apply", self.wvar, if set { "set" } else { "unset" }));
        done(cut(&t, DRY_MAX))
    }

    /// Journals an applied write and answers in one line.
    fn finish(&self, o: Outcome) -> Res<Reply> {
        let unknown = o.status == "unknown";
        let sha = crate::sha256::hex(o.plan.body.as_bytes());
        let handle = self.handle("sent", &o.plan.body).unwrap_or_else(|| "none".into());
        let site = origin(&o.plan.url);
        let word = self.a.verb.word();
        let meta = format!(
            "verb: {word}\ntarget: {}\nsite: {site}\nstatus: {}\nresult: {}\nbody_sha: {sha}\nhandle: {handle}\nat: {}",
            self.a.target, o.status, if unknown { "unknown" } else { &o.result }, self.env.now_s
        );
        let title = format!("track {word} {}: {}", self.a.target, o.result);
        let d = Draft {
            title: cut(&title, 200),
            meta,
            body: format!("{} {} sent; {}", o.plan.method.name(), o.plan.shown, o.result),
            reason: "an applied project tracking write (atlas track)".into(),
            author: "atlas".into(),
            client: "atlas-track".into(),
            provenance: None,
            time: None,
        };
        let key = (|| -> crate::Result<String> {
            let mut store = MemoryStore::open(self.ctx.store)?;
            let mut w = store.writer()?;
            let key = w.next_key(&format!("journal:{}:", self.lane), 4)?;
            w.create(&key, "journal", "note", &self.lane, &d, None)?;
            w.commit()?;
            Ok(key)
        })();
        match key {
            Ok(key) if unknown => Ok(fail(format!(
                "{}: outcome unknown: check with atlas track {} {} (the write may have been applied); {key}",
                self.who,
                if self.a.verb.confluence() { "page" } else { "issue" },
                self.a.target
            ))),
            Ok(key) => {
                let tail = format!(" {key}");
                let head = format!("{} ", o.subject);
                Ok(done(format!("{head}{}{tail}", cut(&o.result, REPLY_MAX.saturating_sub(head.len() + tail.len())))))
            }
            Err(e) => Ok(fail(format!("{}: sent ({}), but the journal entry failed: {e}", self.who, o.result))),
        }
    }

    /// The outcome of a write that may or may not have happened.
    fn unknown(&self, plan: Plan, why: &str) -> Res<Reply> {
        let _ = why;
        self.finish(Outcome { subject: self.a.target.clone(), result: "unknown".into(), status: "unknown".into(), plan })
    }

    // ---- comment

    /// The newest journal entry for this same comment, and its age.
    fn recent_duplicate(&self, sha: &str) -> Option<(String, u64)> {
        let store = MemoryStore::open(self.ctx.store).ok()?;
        let meta = |m: &str, k: &str| m.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()));
        let found = store.lane(&self.lane).ok()?.into_iter().rev().find(|h| {
            h.title.starts_with("track comment ")
                && meta(&h.meta, "target:").as_deref() == Some(self.a.target.as_str())
                && meta(&h.meta, "body_sha:").as_deref() == Some(sha)
        })?;
        let at: u64 = meta(&found.meta, "at:")?.parse().ok()?;
        let age = self.env.now_s.saturating_sub(at);
        (age < DUPLICATE_WINDOW_S).then_some((found.key, age))
    }

    fn comment(&self) -> Res<Reply> {
        let text = self.a.body.as_deref().unwrap_or("").trim_end();
        let payload = if self.cloud() { json!({"body": adf(text)}) } else { json!({"body": text}) };
        let e = write_ep("comment");
        let plan = Plan { method: e.method, url: self.jira_path_of(e), body: json::to_string(&payload), shown: shown(e, self.cloud()) };
        if !self.a.again {
            if let Some((key, age)) = self.recent_duplicate(&crate::sha256::hex(plan.body.as_bytes())) {
                return Ok(refuse(format!(
                    "{}: the same comment was added {age} s ago ({key}); add --again to send it anyway; nothing sent",
                    self.who
                )));
            }
        }
        if !self.a.apply {
            return Ok(self.dry(&plan, &[]));
        }
        match self.send(&plan) {
            Sent::Resp(r) if (200..300).contains(&r.status) => {
                let id = json::parse(&String::from_utf8_lossy(&r.body)).map(|v| text_of(&v, "/id")).unwrap_or_default();
                let result = if id.is_empty() { "comment added".to_string() } else { format!("comment {id} added") };
                self.finish(Outcome { subject: self.a.target.clone(), result, status: r.status.to_string(), plan })
            }
            Sent::Resp(r) if r.status >= 500 => self.unknown(plan, "server error"),
            Sent::Resp(r) => Ok(self.http_fail(&plan, &r, self.s.jira_base())),
            Sent::Down(f) => self.unknown(plan, &f.describe(self.s.timeout())),
        }
    }

    fn jira_path_of(&self, e: &WriteEndpoint) -> String {
        let p = if self.cloud() { e.cloud } else { e.dc };
        format!("{}{}", self.s.jira_base(), p.replace("{key}", &enc(&self.a.target)))
    }

    // ---- transition

    /// The issue's status: its name and id.
    fn status(&self) -> Res<(String, String)> {
        let v = self.read("issue_state", self.s.jira_base(), &format!("{}?fields=status", self.jira_path("issue_state").trim_start_matches(self.s.jira_base())))?;
        Ok((text_of(&v, "/fields/status/name"), text_of(&v, "/fields/status/id")))
    }

    fn transition(&self) -> Res<Reply> {
        let to = self.a.to.as_deref().unwrap_or("").trim();
        // The allow-list read asked for the status too: no second GET.
        let (cur, cur_id) = (text_of(&self.issue, "/fields/status/name"), text_of(&self.issue, "/fields/status/id"));
        if eq_ic(&cur, to) || (!cur_id.is_empty() && cur_id == to) {
            return Ok(done(format!("{}: already {cur}; nothing sent", self.a.target)));
        }
        let list = self.read("transitions", self.s.jira_base(), self.jira_path("transitions").trim_start_matches(self.s.jira_base()))?;
        let all: Vec<&Value> = list.get("transitions").and_then(Value::as_array).map(|v| v.iter().collect()).unwrap_or_default();
        if all.is_empty() {
            return Ok(refuse(format!("{}: no transitions visible to this token's user; nothing sent", self.who)));
        }
        let mut hits: Vec<(String, String, String)> = Vec::new();
        for t in &all {
            let (id, name, dest, dest_id) = (text_of(t, "/id"), text_of(t, "/name"), text_of(t, "/to/name"), text_of(t, "/to/id"));
            if (id == to || eq_ic(&name, to) || eq_ic(&dest, to) || (!dest_id.is_empty() && dest_id == to)) && !hits.iter().any(|h| h.0 == id) {
                hits.push((id, name, dest));
            }
        }
        let [(id, name, dest)] = hits.as_slice() else {
            let mut avail: Vec<String> = Vec::new();
            for t in &all {
                let d = text_of(t, "/to/name");
                if !avail.contains(&d) {
                    avail.push(d);
                }
            }
            return Ok(refuse(if hits.is_empty() {
                format!("{}: no transition to {to:?} (available: {}); nothing sent", self.who, avail.join(", "))
            } else {
                format!("{}: {} transitions match {to:?}; give a transition id; nothing sent", self.who, hits.len())
            }));
        };
        let e = write_ep("transition");
        let plan = Plan {
            method: e.method,
            url: self.jira_path_of(e),
            body: json::to_string(&json!({"transition": {"id": id.clone()}})),
            shown: shown(e, self.cloud()),
        };
        let result = format!("{cur} -> {dest}");
        if !self.a.apply {
            return Ok(self.dry(&plan, &[format!("transition {id} ({name}): {result}")]));
        }
        let (mut tries, mut conflict) = (0u32, false);
        let ok = |status: String, plan: Plan| self.finish(Outcome { subject: self.a.target.clone(), result: result.clone(), status, plan });
        loop {
            let why = match self.send(&plan) {
                Sent::Resp(r) if (200..300).contains(&r.status) => return ok(r.status.to_string(), plan),
                Sent::Resp(r) if r.status == 409 && !conflict => {
                    conflict = true;
                    String::from("conflicting update")
                }
                Sent::Resp(r) if r.status >= 500 => format!("HTTP {}", r.status),
                Sent::Resp(r) => return Ok(self.http_fail(&plan, &r, self.s.jira_base())),
                Sent::Down(f) => f.describe(self.s.timeout()),
            };
            // Re-read before anything else: only an unchanged status may be retried.
            let (now, _) = self.status()?;
            if eq_ic(&now, dest) {
                return ok("confirmed".into(), plan);
            }
            if !eq_ic(&now, &cur) || tries >= self.s.max_retries {
                return self.unknown(plan, &why);
            }
            tries += 1;
            self.backoff(tries);
        }
    }

    // ---- edit

    fn edit(&self) -> Res<Reply> {
        let a = self.a;
        let mut fields = Map::new();
        let mut update = Map::new();
        let mut parts: Vec<String> = Vec::new();
        if !a.labels_add.is_empty() || !a.labels_remove.is_empty() {
            let mut ops: Vec<Value> = a.labels_add.iter().map(|l| json!({"add": l.clone()})).collect();
            ops.extend(a.labels_remove.iter().map(|l| json!({"remove": l.clone()})));
            update.insert("labels".into(), Value::Array(ops));
            let mut p = String::from("labels");
            for l in &a.labels_add {
                p.push_str(&format!(" +{l}"));
            }
            for l in &a.labels_remove {
                p.push_str(&format!(" -{l}"));
            }
            parts.push(p);
        }
        if let Some(who) = &a.assignee {
            let key = if self.cloud() { "accountId" } else { "name" };
            let id = match who.as_str() {
                "none" => Value::Null,
                "me" => {
                    let v = self.read("myself", self.s.jira_base(), self.jira_path("myself").trim_start_matches(self.s.jira_base()))?;
                    Value::String(text_of(&v, &format!("/{key}")))
                }
                id if self.cloud() && account_id_shaped(id) => Value::String(id.to_string()),
                q => {
                    let param = if self.cloud() { "query" } else { "username" };
                    let path = format!("{}?issueKey={}&{param}={}", self.jira_path("assignable").split('?').next().unwrap_or(""), enc(&a.target), enc(q));
                    let v = self.read("assignable", self.s.jira_base(), path.trim_start_matches(self.s.jira_base()))?;
                    let users = v.as_array().map(Vec::as_slice).unwrap_or_default();
                    let [u] = users else {
                        return Ok(refuse(format!(
                            "{}: --assignee {q:?}: {} accounts match; give an account id; nothing sent",
                            self.who,
                            users.len()
                        )));
                    };
                    Value::String(text_of(u, &format!("/{key}")))
                }
            };
            fields.insert("assignee".into(), Value::Object(std::iter::once((key.to_string(), id)).collect()));
            parts.push(format!("assignee {who}"));
        }
        if let Some(sp) = &a.sprint {
            let list = self.read("fields", self.s.jira_base(), self.jira_path("fields").trim_start_matches(self.s.jira_base()))?;
            let id = list
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| text_of(f, "/schema/custom") == "com.pyxis.greenhopper.jira:gh-sprint")
                .map(|f| text_of(f, "/id"))
                .filter(|i| !i.is_empty());
            let Some(id) = id else {
                return Ok(refuse(format!("{}: no Sprint field found in this Jira's field list; nothing sent", self.who)));
            };
            let value = if sp == "none" { Value::Null } else { Value::from(sp.parse::<u64>().unwrap_or(0)) };
            fields.insert(id, value);
            parts.push(format!("sprint {sp}"));
        }
        let mut body = Map::new();
        if !fields.is_empty() {
            body.insert("fields".into(), Value::Object(fields));
        }
        if !update.is_empty() {
            body.insert("update".into(), Value::Object(update));
        }
        let e = write_ep("edit");
        let plan = Plan { method: e.method, url: self.jira_path_of(e), body: json::to_string(&Value::Object(body)), shown: shown(e, self.cloud()) };
        if !a.apply {
            return Ok(self.dry(&plan, &[format!("edit: {}", parts.join(", "))]));
        }
        let (mut tries, mut conflict) = (0u32, false);
        loop {
            match self.send(&plan) {
                Sent::Resp(r) if (200..300).contains(&r.status) => {
                    return self.finish(Outcome { subject: a.target.clone(), result: parts.join(", "), status: r.status.to_string(), plan });
                }
                Sent::Resp(r) if r.status == 409 && !conflict => {
                    conflict = true;
                    self.status()?;
                }
                Sent::Resp(r) if r.status >= 500 && tries < self.s.max_retries => {
                    tries += 1;
                    self.backoff(tries);
                }
                Sent::Resp(r) if r.status >= 500 => return self.unknown(plan, "server error"),
                Sent::Resp(r) => return Ok(self.http_fail(&plan, &r, self.s.jira_base())),
                Sent::Down(f) => return self.unknown(plan, &f.describe(self.s.timeout())),
            }
        }
    }

    // ---- page update

    /// The page: its version, title, space (id or key) and storage body.
    fn read_page(&self) -> Res<Page> {
        let c = self.client()?;
        let got = c.page(&self.a.target).map_err(|e| fail(format!("{}: {e}", self.who)))?;
        let v = got.value;
        Ok(Page {
            version: v.pointer("/version/number").and_then(Value::as_u64).unwrap_or(0),
            title: text_of(&v, "/title"),
            space: if self.cloud() { text_of(&v, "/spaceId") } else { text_of(&v, "/space/key") },
            body: text_of(&v, "/body/storage/value"),
        })
    }

    /// Refuses a page whose space the allow-list does not name.
    fn allow_space(&self, space: &str) -> Res<()> {
        let list = &self.s.write.confluence;
        let find = |k: &str| list.iter().find(|(s, _)| s == k);
        let mut key = String::new();
        let mut entry = find(space);
        if entry.is_none() && self.cloud() {
            // The allow-list may name the space by its key; the page carries its id.
            let base = self.conf_base()?;
            let e = pre_read("space");
            let path = e.cloud.replace("{id}", &enc(space));
            if let Ok(g) = self.client()?.get(e, &base, &path) {
                key = text_of(&g.value, "/key");
                entry = find(&key);
            }
        }
        if entry.is_some_and(|(_, l)| l.iter().any(|v| v == "page_update")) {
            return Ok(());
        }
        let name = if key.is_empty() { format!("space {space}") } else { format!("space {space} ({key})") };
        let why = match entry {
            None => format!("project_tracking.atlassian.writes.confluence has no {}", if key.is_empty() { space.to_string() } else { format!("{key} or {space}") }),
            // CONFLUENCE_VERBS is only page_update, so a listed space that lacks it lists nothing.
            Some((k, _)) => format!("project_tracking.atlassian.writes.confluence.{k} allows: nothing"),
        };
        Err(refuse(format!("{}: refused: page_update is not allowed for Confluence {name} ({why}); nothing sent", self.who)))
    }

    fn page_update(&self) -> Res<Reply> {
        let a = self.a;
        let base = self.conf_base()?;
        let page = self.read_page()?;
        self.allow_space(&page.space)?;
        if let Some(n) = a.expect_version {
            if n != page.version {
                return Ok(refuse(format!(
                    "{}: expected version {n} but the page is at version {}; nothing sent; re-read it and decide",
                    self.who, page.version
                )));
            }
        }
        let given = a.body.as_deref().unwrap_or("");
        let (new_body, what, old_len, new_len) = match (&a.section, a.append) {
            (Some(h), append) => {
                let e = section_edit(&page.body, h, given, append).map_err(|m| refuse(format!("{}: {m}; nothing sent", self.who)))?;
                (e.body, format!("section {}", h.trim()), e.old_len, e.new_len)
            }
            (None, true) => (format!("{}{given}", page.body), String::new(), page.body.len(), page.body.len() + given.len()),
            (None, false) => (given.to_string(), String::new(), page.body.len(), given.len()),
        };
        let next = page.version + 1;
        let title = a.title.clone().unwrap_or_else(|| page.title.clone());
        let message = a.message.clone().unwrap_or_else(|| "atlas track page-update".into());
        let payload = if self.cloud() {
            json!({"id": a.target.clone(), "status": "current", "title": title,
                   "body": {"representation": "storage", "value": new_body.clone()},
                   "version": {"number": next, "message": message}})
        } else {
            json!({"id": a.target.clone(), "type": "page", "title": title,
                   "version": {"number": next, "message": message},
                   "body": {"storage": {"value": new_body.clone(), "representation": "storage"}}})
        };
        let e = write_ep("page_update");
        let path = (if self.cloud() { e.cloud } else { e.dc }).replace("{id}", &enc(&a.target));
        let plan = Plan { method: e.method, url: format!("{base}{path}"), body: json::to_string(&payload), shown: shown(e, self.cloud()) };
        let sizes = if what.is_empty() { format!("body {old_len} B -> {new_len} B") } else { format!("{what}: {old_len} B -> {new_len} B") };
        if !a.apply {
            return Ok(self.dry(&plan, &[format!("page {} {:?}: version {} -> {next}", a.target, page.title, page.version), sizes]));
        }
        let mut result = format!("v{} -> v{next}", page.version);
        if !what.is_empty() {
            result.push_str(&format!(" {what}"));
        }
        result.push_str(&format!(" (+{new_len}/-{old_len} B)"));
        let mut tries = 0u32;
        loop {
            let why = match self.send(&plan) {
                Sent::Resp(r) if (200..300).contains(&r.status) => {
                    return self.finish(Outcome { subject: format!("page {}", a.target), result, status: r.status.to_string(), plan });
                }
                Sent::Resp(r) if r.status == 409 => {
                    let now = self.read_page().map_or_else(|_| "unknown".to_string(), |p| p.version.to_string());
                    return Ok(fail(format!(
                        "page {}: not written: the page changed since it was read (now version {now}, sent {next}) or the space needs approval before publishing; re-run to work from version {now}",
                        a.target
                    )));
                }
                Sent::Resp(r) if r.status >= 500 => format!("HTTP {}", r.status),
                Sent::Resp(r) => return Ok(self.http_fail(&plan, &r, &base)),
                Sent::Down(f) => f.describe(self.s.timeout()),
            };
            // A retry only while the page is still at the version that was read.
            let now = self.read_page()?;
            if now.version != page.version || tries >= self.s.max_retries {
                let msg = format!(
                    "{}: outcome unknown: the page is now at version {} (read at {}); check with atlas track page {} ({why})",
                    self.who, now.version, page.version, a.target
                );
                let journaled = self.unknown(plan, &why)?;
                let key = journaled.text.rsplit(' ').next().unwrap_or("");
                return Ok(fail(format!("{msg}; {key}")));
            }
            tries += 1;
            self.backoff(tries);
        }
    }
}

struct Page {
    version: u64,
    title: String,
    space: String,
    body: String,
}

/// The endpoint as errors name it: the method and the path template.
fn shown(e: &WriteEndpoint, cloud: bool) -> String {
    let pre = if cloud && e.product == Product::Confluence { "/wiki" } else { "" };
    format!("{} {pre}{}", e.method.name(), if cloud { e.cloud } else { e.dc })
}

/// True for the shapes of a Cloud account id (24 hex digits, or `NNN:uuid`).
fn account_id_shaped(s: &str) -> bool {
    let hex = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    (s.len() == 24 && s.bytes().all(|b| b.is_ascii_alphanumeric()) && s.bytes().any(|b| b.is_ascii_digit()))
        || s.split_once(':').is_some_and(|(n, u)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && hex(u))
}
