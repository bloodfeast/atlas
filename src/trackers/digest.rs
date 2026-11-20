//! Digests of Jira and Confluence responses (intent:r01607): a Jira issue
//! trimmed to the fields an agent uses, a JQL search as one row per issue,
//! a Confluence page as its metadata and the outline of its body as
//! markdown, a CQL search as one row per hit, and a generic JSON outline for
//! anything else.
//!
//! The handle keeps the whole response pretty-printed (so `lines=` and
//! `grep=` work), followed by each rich-text body converted to markdown
//! under a `--- markdown: NAME ---` line: a Jira description (Atlassian
//! Document Format on Cloud, wiki markup on Data Center) or a Confluence
//! page body (storage XHTML). Every function is a pure function of its
//! input, as [`crate::digest`]'s are.

use crate::json::{self, Value};

/// What a response is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A Jira issue (`key` and `fields`).
    Issue,
    /// A JQL search (`issues`).
    Search,
    /// A Confluence page (`title` and `body`).
    Page,
    /// A CQL search (`results`).
    Cql,
    /// Anything else: the generic outline.
    Other,
}

/// The kind of a parsed response.
pub fn kind_of(v: &Value) -> Kind {
    let has = |k: &str| v.get(k).is_some();
    if v.get("key").is_some_and(Value::is_string) && v.get("fields").is_some_and(Value::is_object) {
        Kind::Issue
    } else if v.get("issues").is_some_and(Value::is_array) {
        Kind::Search
    } else if has("title") && v.get("body").is_some_and(Value::is_object) {
        Kind::Page
    } else if v.get("results").is_some_and(Value::is_array)
        && (has("totalSize") || has("cqlQuery") || has("searchDuration") || has("_links"))
    {
        Kind::Cql
    } else {
        Kind::Other
    }
}

/// The line that opens a markdown section of the stored text.
fn mark(name: &str) -> String {
    format!("--- markdown: {name} ---")
}

/// The text a handle keeps for a response: the JSON pretty-printed, then
/// each rich-text body as markdown.
pub fn stored_text(v: &Value) -> String {
    let mut s = json::to_string_pretty(v);
    s.push('\n');
    let md = match kind_of(v) {
        Kind::Issue => v.pointer("/fields/description").map(rich_md).filter(|m| !m.trim().is_empty()).map(|m| ("description", m)),
        Kind::Page => page_body_md(v).map(|m| ("body", m)),
        _ => None,
    };
    if let Some((name, m)) = md {
        s.push_str(&mark(name));
        s.push('\n');
        s.push_str(m.trim_end());
        s.push('\n');
    }
    s
}

/// A markdown section of the stored text: its name, its first line in the
/// stored text (1-based) and its text.
struct Section<'a> {
    name: &'a str,
    first_line: usize,
    text: &'a str,
}

/// Splits stored text into the JSON and its markdown section, if any.
fn split(text: &str) -> (&str, Option<Section<'_>>) {
    let Some(at) = text.find("\n--- markdown: ") else { return (text, None) };
    let rest = &text[at + 1..];
    let (head, body) = rest.split_once('\n').unwrap_or((rest, ""));
    let name = head.trim_start_matches("--- markdown: ").trim_end_matches(" ---");
    let first_line = text[..at + 1].lines().count() + 2;
    (&text[..at], Some(Section { name, first_line, text: body }))
}

/// The items of stored text, for `item=NAME`: `json`, the response alone
/// (it parses as JSON on its own), and `NAME-md` (`description-md`,
/// `body-md`), the markdown rendering, when there is one.
pub fn items(text: &str) -> Vec<crate::digest::Item> {
    let item = |name: String, first: usize, last: usize| crate::digest::Item { name, first, last, status: "section".into(), depth: 0 };
    let (j, sec) = split(text);
    let mut v = vec![item("json".into(), 1, crate::digest::line_count(j).max(1))];
    if let Some(sec) = sec {
        let n = crate::digest::line_count(sec.text);
        if n > 0 {
            v.push(item(format!("{}-md", sec.name), sec.first_line, sec.first_line + n - 1));
        }
    }
    v
}

/// A string cut to `n` characters, whitespace runs made one space.
fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        return one;
    }
    let mut t: String = one.chars().take(n).collect();
    t.push_str("...");
    t
}

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or("")
}

fn or<'a>(x: &'a str, d: &'a str) -> &'a str {
    if x.is_empty() {
        d
    } else {
        x
    }
}

fn names(v: Option<&Value>, key: &str) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.get(key).and_then(Value::as_str).or_else(|| x.as_str())).map(str::to_string).collect())
        .unwrap_or_default()
}

fn date(x: &str) -> &str {
    x.get(..10).unwrap_or(x)
}

/// The digest body of stored text (the header line is
/// [`crate::digest::digest`]'s): by kind, then one hint line.
pub fn body(id: &str, text: &str, max_bytes: usize) -> String {
    #[cfg(feature = "test-seams")]
    if std::env::var_os("ATLAS_TEST_TRACKER_PANIC").is_some() {
        panic!("test seam: a panic in the tracker digest");
    }
    let (j, sec) = split(text);
    let parsed = json::parse(j);
    // A search's rows past the digest's are found by digest-grep.
    let searched = parsed.as_ref().is_ok_and(|v| matches!(kind_of(v), Kind::Search | Kind::Cql));
    let mut out = match parsed {
        Ok(v) => match kind_of(&v) {
            Kind::Issue => issue(&v, sec.as_ref()),
            Kind::Search => search(&v),
            Kind::Page => page(&v, sec.as_ref()),
            Kind::Cql => cql(&v),
            Kind::Other => outline(&v),
        },
        Err(_) => "not JSON: read it by lines\n".to_string(),
    };
    if out.len() > max_bytes {
        let mut cut = max_bytes;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        let end = out[..cut].rfind('\n').map_or(cut, |p| p + 1);
        out.truncate(end);
        out.push_str("... (digest cut; the rest by lines or grep)\n");
    }
    let md = sec.map_or(String::new(), |s| format!(" | item={}-md", s.name));
    let rows = if searched { " | digest-grep=RE" } else { "" };
    out.push_str(&format!("recall {id} lines=A-B | grep=RE | item=json{md}{rows}\n"));
    out
}

fn issue(v: &Value, sec: Option<&Section<'_>>) -> String {
    let f = v.get("fields").unwrap_or(&Value::Null);
    let mut o = format!(
        "{} [{}] {}, priority {}, assignee {}, reporter {}\n",
        s(v, "/key"),
        or(s(f, "/issuetype/name"), "?"),
        or(s(f, "/status/name"), "?"),
        or(s(f, "/priority/name"), "none"),
        or(s(f, "/assignee/displayName"), "unassigned"),
        or(s(f, "/reporter/displayName"), "?"),
    );
    o.push_str(&format!("summary: {}\n", clip(s(f, "/summary"), 200)));
    let mut facts = vec![format!("created {}, updated {}", date(s(f, "/created")), date(s(f, "/updated")))];
    for (label, list) in [
        ("labels", names(f.get("labels"), "")),
        ("components", names(f.get("components"), "name")),
        ("fix versions", names(f.get("fixVersions"), "name")),
    ] {
        if !list.is_empty() {
            facts.push(format!("{label} {}", list.join(", ")));
        }
    }
    if !s(f, "/parent/key").is_empty() {
        facts.push(format!("parent {}", s(f, "/parent/key")));
    }
    if let Some(sprint) = sprint(f) {
        facts.push(format!("sprint {sprint}"));
    }
    o.push_str(&facts.join("; "));
    o.push('\n');
    if let Some(sec) = sec.filter(|x| x.name == "description") {
        let words = sec.text.split_whitespace().count();
        let first = sec.text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("");
        o.push_str(&format!(
            "description ({words} words, lines {}-{}): {}\n",
            sec.first_line,
            sec.first_line + sec.text.lines().count().saturating_sub(1),
            clip(first, 240)
        ));
        o.push_str(&headings(sec));
    } else {
        o.push_str("description: none\n");
    }
    let comments = f.pointer("/comment/comments").and_then(Value::as_array);
    let total = f.pointer("/comment/total").and_then(Value::as_u64).unwrap_or_else(|| comments.map_or(0, |c| c.len() as u64));
    let mut c = format!("comments: {total}");
    if let Some(cs) = comments {
        let last: Vec<String> = cs
            .iter()
            .rev()
            .take(2)
            .rev()
            .map(|x| {
                format!(
                    "{} {}: {}",
                    or(s(x, "/author/displayName"), "?"),
                    date(s(x, "/created")),
                    clip(rich_md(x.get("body").unwrap_or(&Value::Null)).lines().find(|l| !l.trim().is_empty()).unwrap_or(""), 100)
                )
            })
            .collect();
        if !last.is_empty() {
            c.push_str(&format!("; last: {}", last.join(" | ")));
        }
    }
    o.push_str(&c);
    o.push('\n');
    let links: Vec<String> = f
        .get("issuelinks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|l| {
                    let (rel, other) = if l.get("outwardIssue").is_some() {
                        (s(l, "/type/outward"), l.get("outwardIssue"))
                    } else {
                        (s(l, "/type/inward"), l.get("inwardIssue"))
                    };
                    let other = other.unwrap_or(&Value::Null);
                    format!("{rel} {} ({})", s(other, "/key"), or(s(other, "/fields/status/name"), "?"))
                })
                .collect()
        })
        .unwrap_or_default();
    if !links.is_empty() {
        o.push_str(&format!("links: {}\n", links.join("; ")));
    }
    let att = f.get("attachment").and_then(Value::as_array).map_or(0, Vec::len);
    if att > 0 {
        o.push_str(&format!("attachments: {att}\n"));
    }
    let custom: Vec<&str> = f
        .as_object()
        .map(|m| m.iter().filter(|(k, x)| k.starts_with("customfield_") && !is_empty(x)).map(|(k, _)| k.as_str()).collect())
        .unwrap_or_default();
    if !custom.is_empty() {
        let shown: Vec<&str> = custom.iter().take(12).copied().collect();
        let more = if custom.len() > 12 { format!(" (+{} more)", custom.len() - 12) } else { String::new() };
        o.push_str(&format!("custom fields set: {}{more}\n", shown.join(", ")));
    }
    o
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(x) => x.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// The active (else last) sprint's name: a custom field holding objects
/// with `boardId` and `state`.
fn sprint(f: &Value) -> Option<String> {
    let m = f.as_object()?;
    for (k, x) in m.iter() {
        if !k.starts_with("customfield_") {
            continue;
        }
        let Some(a) = x.as_array() else { continue };
        if a.iter().all(|e| e.get("boardId").is_some() && e.get("state").is_some()) && !a.is_empty() {
            let pick = a.iter().find(|e| s(e, "/state") == "active").or_else(|| a.last())?;
            return Some(s(pick, "/name").to_string());
        }
    }
    None
}

/// Each heading of a markdown section with its stored line range and words.
fn headings(sec: &Section<'_>) -> String {
    let items = crate::digest::outline_as(crate::digest::Lang::Markdown, sec.text);
    let lines: Vec<&str> = sec.text.lines().collect();
    let mut o = String::new();
    for it in items.iter().take(30) {
        let words: usize = lines
            .get(it.first.saturating_sub(1)..it.last.min(lines.len()))
            .map_or(0, |ls| ls.iter().map(|l| l.split_whitespace().count()).sum());
        o.push_str(&format!(
            "  {}{} (lines {}-{}, {words} words)\n",
            "  ".repeat(it.depth.saturating_sub(1)),
            clip(&it.name, 70),
            it.first + sec.first_line - 1,
            it.last + sec.first_line - 1
        ));
    }
    if items.len() > 30 {
        o.push_str(&format!("  ... and {} more headings\n", items.len() - 30));
    }
    o
}

/// A JQL search's row for one issue.
fn issue_row(i: &Value) -> String {
    format!(
        "{} {} [{}] {} {} {} {}",
        s(i, "/key"),
        or(s(i, "/fields/status/name"), "?"),
        or(s(i, "/fields/issuetype/name"), "?"),
        or(s(i, "/fields/priority/name"), "-"),
        or(s(i, "/fields/assignee/displayName"), "unassigned"),
        date(s(i, "/fields/updated")),
        clip(s(i, "/fields/summary"), 80)
    )
}

/// A CQL search's row for one hit, its excerpt on a second line when it
/// has one.
fn hit_row(r: &Value) -> String {
    let id = or(s(r, "/content/id"), s(r, "/id"));
    let ty = or(s(r, "/content/type"), or(s(r, "/entityType"), "?"));
    let space = or(s(r, "/resultGlobalContainer/title"), or(s(r, "/content/space/key"), "-"));
    let title = or(s(r, "/title"), s(r, "/content/title"));
    let mut o = format!(
        "{id} {ty} {} {} {}",
        clip(space, 30),
        clip(&title.replace("@@@hl@@@", "").replace("@@@endhl@@@", ""), 70),
        date(s(r, "/lastModified"))
    );
    let ex = s(r, "/excerpt").replace("@@@hl@@@", "").replace("@@@endhl@@@", "");
    if !ex.trim().is_empty() {
        o.push_str(&format!("\n  {}", clip(&ex, 100)));
    }
    o
}

/// Every row of a search's digest, for `digest-grep=RE`: one per issue of
/// a JQL search or hit of a CQL search, not only the 30 the digest shows.
/// `None` for any other response (its digest lines are its rows).
pub fn rows(text: &str) -> Option<Vec<String>> {
    let v = json::parse(split(text).0).ok()?;
    let all = |k: &str, f: fn(&Value) -> String| v.get(k).and_then(Value::as_array).map(|a| a.iter().map(f).collect::<Vec<_>>());
    match kind_of(&v) {
        Kind::Search => all("issues", issue_row),
        Kind::Cql => all("results", hit_row),
        _ => None,
    }
}

fn search(v: &Value) -> String {
    let issues = v.get("issues").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let more = match (v.get("isLast").and_then(Value::as_bool), v.get("total").and_then(Value::as_u64)) {
        (Some(last), _) => {
            if last {
                "no".to_string()
            } else {
                "yes".to_string()
            }
        }
        (None, Some(t)) => {
            if (issues.len() as u64) < t {
                format!("yes ({t} in all)")
            } else {
                "no".to_string()
            }
        }
        _ => "unknown".to_string(),
    };
    let mut o = format!("{} issues, more pages: {more}\n", issues.len());
    for i in issues.iter().take(30) {
        o.push_str(&issue_row(i));
        o.push('\n');
    }
    if issues.len() > 30 {
        o.push_str(&format!("... and {} more (digest-grep=KEY)\n", issues.len() - 30));
    }
    o
}

fn page(v: &Value, sec: Option<&Section<'_>>) -> String {
    let space = or(s(v, "/spaceId"), s(v, "/space/key"));
    let version = v.pointer("/version/number").and_then(Value::as_u64).map_or("?".to_string(), |n| n.to_string());
    let by = or(s(v, "/version/by/displayName"), s(v, "/version/authorId"));
    let when = or(s(v, "/version/createdAt"), s(v, "/version/when"));
    let mut o = format!(
        "\"{}\" id {}, space {}, version {version} by {} {}\n",
        clip(s(v, "/title"), 100),
        s(v, "/id"),
        or(space, "?"),
        or(by, "?"),
        date(when)
    );
    let web = s(v, "/_links/webui");
    if !web.is_empty() {
        o.push_str(&format!("url: {}{web}\n", s(v, "/_links/base")));
    }
    match sec.filter(|x| x.name == "body") {
        Some(sec) => {
            let words = sec.text.split_whitespace().count();
            let first = sec.text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("");
            o.push_str(&format!(
                "body as markdown ({words} words, lines {}-{}): {}\n",
                sec.first_line,
                sec.first_line + sec.text.lines().count().saturating_sub(1),
                clip(first, 240)
            ));
            o.push_str(&headings(sec));
        }
        None => o.push_str("body: none\n"),
    }
    o
}

fn cql(v: &Value) -> String {
    let rs = v.get("results").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let total = v.get("totalSize").and_then(Value::as_u64).map_or("?".to_string(), |t| t.to_string());
    let next = if s(v, "/_links/next").is_empty() { "no" } else { "yes" };
    let mut o = format!("{} hits of {total}, more pages: {next}\n", rs.len());
    for r in rs.iter().take(30) {
        o.push_str(&hit_row(r));
        o.push('\n');
    }
    if rs.len() > 30 {
        o.push_str(&format!("... and {} more (digest-grep=TITLE)\n", rs.len() - 30));
    }
    o
}

/// One value's shape for the outline.
fn shape(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(_) => json::to_string(v),
        Value::String(x) => format!("\"{}\"", clip(x, 80)),
        Value::Array(a) => {
            let keys: Vec<String> = a.first().and_then(Value::as_object).map(|m| m.keys().take(8).cloned().collect()).unwrap_or_default();
            if keys.is_empty() {
                format!("[{} items]", a.len())
            } else {
                format!("[{} objects: {}]", a.len(), keys.join(", "))
            }
        }
        Value::Object(m) => format!("{{{} keys: {}}}", m.len(), m.keys().take(8).cloned().collect::<Vec<_>>().join(", ")),
    }
}

/// The generic JSON outline: each top-level key and its value's shape.
pub fn outline(v: &Value) -> String {
    match v.as_object() {
        Some(m) => {
            let mut o = format!("JSON object, {} keys\n", m.len());
            for (k, x) in m.iter().take(40) {
                o.push_str(&format!("{k}: {}\n", shape(x)));
            }
            if m.len() > 40 {
                o.push_str(&format!("... and {} more keys\n", m.len() - 40));
            }
            o
        }
        None => format!("JSON {}\n", shape(v)),
    }
}

/// A rich-text value as markdown: Atlassian Document Format (an object
/// with `type: doc`), or a string (wiki markup or plain text) as it is.
pub fn rich_md(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Object(_) => {
            let mut o = String::new();
            adf(v, &mut o, 0);
            tidy(&o)
        }
        _ => String::new(),
    }
}

/// Collapses runs of blank lines and trims the ends.
fn tidy(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for l in s.lines() {
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l.trim_end());
        out.push('\n');
    }
    out.trim().to_string() + "\n"
}

fn adf_children(v: &Value, o: &mut String, depth: usize) {
    if let Some(cs) = v.get("content").and_then(Value::as_array) {
        for c in cs {
            adf(c, o, depth);
        }
    }
}

fn adf(v: &Value, o: &mut String, depth: usize) {
    let attr = |k: &str| v.pointer(&format!("/attrs/{k}"));
    match s(v, "/type") {
        "text" => {
            let t = s(v, "/text");
            let code = v.get("marks").and_then(Value::as_array).is_some_and(|m| m.iter().any(|x| s(x, "/type") == "code"));
            if code {
                o.push_str(&format!("`{t}`"));
            } else {
                o.push_str(t);
            }
        }
        "hardBreak" => o.push('\n'),
        "paragraph" => {
            adf_children(v, o, depth);
            o.push_str("\n\n");
        }
        "heading" => {
            let level = attr("level").and_then(Value::as_u64).unwrap_or(1).clamp(1, 6) as usize;
            o.push_str(&format!("\n{} ", "#".repeat(level)));
            adf_children(v, o, depth);
            o.push_str("\n\n");
        }
        "bulletList" | "orderedList" => {
            for c in v.get("content").and_then(Value::as_array).into_iter().flatten() {
                o.push_str(&format!("{}- ", "  ".repeat(depth)));
                let mut item = String::new();
                adf_children(c, &mut item, depth + 1);
                o.push_str(item.trim());
                o.push('\n');
            }
            o.push('\n');
        }
        "codeBlock" => {
            o.push_str("```\n");
            adf_children(v, o, depth);
            o.push_str("\n```\n\n");
        }
        "blockquote" => {
            let mut q = String::new();
            adf_children(v, &mut q, depth);
            for l in q.trim().lines() {
                o.push_str(&format!("> {l}\n"));
            }
            o.push('\n');
        }
        "rule" => o.push_str("\n---\n\n"),
        "mention" => o.push_str(attr("text").and_then(Value::as_str).unwrap_or("@someone")),
        "emoji" => o.push_str(attr("shortName").and_then(Value::as_str).unwrap_or("")),
        "inlineCard" | "blockCard" => o.push_str(attr("url").and_then(Value::as_str).unwrap_or("")),
        "status" => o.push_str(&format!("[{}]", attr("text").and_then(Value::as_str).unwrap_or(""))),
        "date" => o.push_str(attr("timestamp").and_then(Value::as_str).unwrap_or("")),
        "tableRow" => {
            o.push('|');
            for c in v.get("content").and_then(Value::as_array).into_iter().flatten() {
                let mut cell = String::new();
                adf_children(c, &mut cell, depth);
                o.push_str(&format!(" {} |", cell.split_whitespace().collect::<Vec<_>>().join(" ")));
            }
            o.push('\n');
        }
        "mediaSingle" | "mediaGroup" | "media" => o.push_str("[media]\n"),
        _ => adf_children(v, o, depth),
    }
}

/// A page's body as markdown: storage XHTML, an ADF document, or a
/// markdown view, whichever the response holds.
pub fn page_body_md(v: &Value) -> Option<String> {
    if let Some(x) = v.pointer("/body/storage/value").and_then(Value::as_str) {
        return Some(storage_md(x));
    }
    if let Some(x) = v.pointer("/body/atlas_doc_format/value").and_then(Value::as_str) {
        return json::parse(x).ok().map(|d| rich_md(&d));
    }
    v.pointer("/body/view/value").and_then(Value::as_str).map(storage_md)
}

/// Decodes the entities storage XHTML uses.
fn entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut o = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        o.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';').filter(|&e| e <= 10);
        let Some(e) = end else {
            o.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..e];
        let c = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "ndash" | "mdash" => Some('-'),
            "rsquo" | "lsquo" => Some('\''),
            "rdquo" | "ldquo" => Some('"'),
            n if n.starts_with("#x") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
            n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                o.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                o.push('&');
                rest = &tail[1..];
            }
        }
    }
    o.push_str(rest);
    o
}

/// Confluence storage XHTML as markdown: headings, paragraphs, lists,
/// code macros (fenced), tables as pipe rows, other tags dropped.
pub fn storage_md(x: &str) -> String {
    let mut o = String::new();
    let mut rest = x;
    let mut list_depth = 0usize;
    let mut in_code = false;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>").unwrap_or(r.len());
            o.push_str(&r[..end]);
            rest = r.get(end + 3..).unwrap_or("");
            continue;
        }
        if rest.starts_with('<') {
            let end = rest.find('>').unwrap_or(rest.len() - 1);
            let tag = &rest[1..end];
            rest = &rest[end + 1..];
            let closing = tag.starts_with('/');
            let name = tag.trim_start_matches('/').split([' ', '/', '\t', '\n']).next().unwrap_or("").to_ascii_lowercase();
            match (name.as_str(), closing) {
                ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", false) => {
                    o.push_str(&format!("\n\n{} ", "#".repeat(usize::from(name.as_bytes()[1] - b'0'))))
                }
                ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => o.push_str("\n\n"),
                ("p" | "div", true) => o.push_str("\n\n"),
                ("p", false) if list_depth == 0 => o.push_str("\n\n"),
                ("br", _) => o.push('\n'),
                ("ul" | "ol", false) => list_depth += 1,
                ("ul" | "ol", true) => {
                    list_depth = list_depth.saturating_sub(1);
                    o.push('\n');
                }
                ("li", false) => o.push_str(&format!("\n{}- ", "  ".repeat(list_depth.saturating_sub(1)))),
                ("tr", false) => o.push_str("\n|"),
                ("td" | "th", true) => o.push_str(" |"),
                ("td" | "th", false) => o.push(' '),
                ("table", true) => o.push_str("\n\n"),
                ("code", false) if !in_code => o.push('`'),
                ("code", true) if !in_code => o.push('`'),
                ("pre", false) | ("ac:plain-text-body", false) => {
                    in_code = true;
                    o.push_str("\n\n```\n");
                }
                ("pre", true) | ("ac:plain-text-body", true) => {
                    in_code = false;
                    o.push_str("\n```\n\n");
                }
                ("hr", _) => o.push_str("\n\n---\n\n"),
                _ => {}
            }
            continue;
        }
        let end = rest.find('<').unwrap_or(rest.len());
        let t = entities(&rest[..end]);
        if in_code {
            o.push_str(&t);
        } else {
            o.push_str(&t.replace(['\n', '\r'], " "));
        }
        rest = &rest[end..];
    }
    tidy(&o.lines().map(str::trim_end).collect::<Vec<_>>().join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/atlassian").join(name);
        json::parse(&std::fs::read_to_string(&p).unwrap()).unwrap()
    }

    fn digest_of(v: &Value) -> (String, String) {
        let text = stored_text(v);
        (
            crate::digest::digest("h:0123456789", "tracker", crate::digest::Shape::Tracker, &text, None, crate::digest::DEFAULT_MAX_BYTES),
            text,
        )
    }

    #[test]
    fn a_jira_issue_is_trimmed_to_the_fields_an_agent_uses() {
        let v = fixture("jira-issue.json");
        assert_eq!(kind_of(&v), Kind::Issue);
        let (d, text) = digest_of(&v);
        assert!(d.contains("PROJ-123 [Bug] In Progress, priority High, assignee Avery Example, reporter Robin Example"), "{d}");
        assert!(d.contains("summary: Checkout fails when the cart holds more than fifty items"), "{d}");
        assert!(d.contains("labels checkout, regression"), "{d}");
        assert!(d.contains("parent PROJ-100"), "{d}");
        assert!(d.contains("sprint Sprint 42"), "{d}");
        assert!(d.contains("comments: 3; last:"), "{d}");
        assert!(d.contains("links: blocks PROJ-130 (To Do)"), "{d}");
        assert!(d.contains("attachments: 2"), "{d}");
        assert!(d.contains("## Steps to reproduce") || d.contains("Steps to reproduce (lines"), "{d}");
        // Trimmed: no avatar URL, no self link, no null custom field names.
        assert!(!d.contains("avatarUrls") && !d.contains("48x48") && !d.contains("customfield_10099"), "{d}");
        assert!(text.contains("--- markdown: description ---") && text.contains("## Steps to reproduce"));
        assert!(d.len() * 4 < text.len(), "the digest is a fraction of the raw: {} of {}", d.len(), text.len());
    }

    #[test]
    fn a_jql_search_is_one_row_per_issue() {
        let v = fixture("jira-search-1.json");
        assert_eq!(kind_of(&v), Kind::Search);
        let (d, _) = digest_of(&v);
        assert!(d.contains("3 issues, more pages: yes"), "{d}");
        assert!(d.contains("PROJ-201 To Do [Story] Medium unassigned 2026-11-02 Export the report as CSV"), "{d}");
    }

    #[test]
    fn a_confluence_page_body_is_markdown_with_an_outline() {
        let v = fixture("confluence-page.json");
        assert_eq!(kind_of(&v), Kind::Page);
        let (d, text) = digest_of(&v);
        assert!(d.contains("\"Release checklist\" id 98765, space 4456, version 7"), "{d}");
        assert!(d.contains("Before the freeze (lines "), "{d}");
        let md = page_body_md(&v).unwrap();
        assert!(
            md.contains("## Before the freeze") && md.contains("- Tag the release branch") && md.contains("```\ncargo xtask release\n```"),
            "{md}"
        );
        assert!(md.contains("Owners & dates") && !md.contains("<p>") && !md.contains("&amp;"), "{md}");
        assert!(text.contains("--- markdown: body ---"));
    }

    #[test]
    fn a_cql_search_is_one_row_per_hit() {
        let v = fixture("confluence-search-1.json");
        assert_eq!(kind_of(&v), Kind::Cql);
        let (d, _) = digest_of(&v);
        assert!(d.contains("2 hits of 3, more pages: yes"), "{d}");
        assert!(d.contains("98765 page Engineering Release checklist 2026-11-10"), "{d}");
        assert!(!d.contains("@@@hl@@@"), "{d}");
    }

    #[test]
    fn anything_else_gets_a_json_outline() {
        let v = fixture("jira-myself.json");
        assert_eq!(kind_of(&v), Kind::Other);
        let (d, _) = digest_of(&v);
        assert!(d.contains("JSON object, ") && d.contains("avatarUrls: {4 keys"), "{d}");
    }

    #[test]
    fn adf_converts_headings_lists_code_and_marks() {
        let doc = json::parse(r#"{"type":"doc","version":1,"content":[{"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"Plan"}]},{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"one "},{"type":"text","text":"x()","marks":[{"type":"code"}]}]}]}]},{"type":"codeBlock","content":[{"type":"text","text":"let a = 1;"}]}]}"#).unwrap();
        assert_eq!(rich_md(&doc), "## Plan\n\n- one `x()`\n\n```\nlet a = 1;\n```\n");
    }

    /// The token cost of raw against digest for each fixture, printed (the
    /// brief's table: `cargo test trackers_token_table -- --nocapture`).
    #[test]
    fn trackers_token_table() {
        let bpt = crate::tokens::DEFAULT_BYTES_PER_TOKEN;
        println!("fixture | raw bytes | raw tokens | digest bytes | digest tokens | ratio");
        for f in ["jira-issue.json", "jira-search-1.json", "confluence-page.json", "confluence-search-1.json", "jira-myself.json"] {
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/atlassian").join(f);
            let raw = std::fs::read_to_string(&p).unwrap();
            let v = json::parse(&raw).unwrap();
            let (d, _) = digest_of(&v);
            let (rt, dt) = (raw.len() as f64 / bpt, d.len() as f64 / bpt);
            println!("{f} | {} | {rt:.0} | {} | {dt:.0} | {:.1}x", raw.len(), d.len(), rt / dt);
            assert!(d.len() < raw.len() || f == "jira-myself.json", "{f}: the digest is not smaller");
        }
    }
}
