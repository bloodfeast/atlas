//! Discussion-thread tools. A child module of `tools`, so it shares the
//! server's private state and helpers.

use crate::json::{Map, Value};
use crate::json;

use super::{arg_str, check_reason, opt_str, short_text, wr, write_text, Server, MAX_LIST};
use crate::discussion::{self as disc, FOLDER, PREFIX};
use crate::error::{Error, Result};
use crate::fsio;
use crate::links;
use crate::markdown;
use crate::paths::{self, Vault};
use crate::store::{NoteRow, WriteFilter, THIS_NOTE};

/// True for a note that is a discussion thread.
pub(super) fn is_thread_row(r: &NoteRow) -> bool {
    links::stem(&r.path).starts_with(PREFIX)
        && links::folder(&r.path) == FOLDER
        && r.frontmatter.contains_key("topic")
        && r.frontmatter.contains_key("posts")
}

fn thread_id(path: &str) -> String {
    links::stem(path).split(' ').next().unwrap_or("").to_string()
}

fn str_list(a: &Map, k: &str, max: usize) -> Result<Vec<String>> {
    match a.get(k) {
        Some(Value::Array(items)) if items.len() <= max => items
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| Error::InvalidArgument(format!("{k} must hold strings"))))
            .collect(),
        Some(Value::Array(_)) => Err(Error::InvalidArgument(format!("{k} holds at most {max} items"))),
        _ => Err(Error::InvalidArgument(format!("{k} must be an array"))),
    }
}

fn one_line(s: &str, what: &str, min: usize, max: usize) -> Result<String> {
    let t = s.trim();
    if t.chars().count() < min || t.len() > max || t.contains(|c: char| c.is_control()) {
        return Err(Error::InvalidArgument(format!("{what} must be one line of {min} to {max} characters")));
    }
    Ok(t.to_string())
}

fn opt_post(a: &Map, k: &str) -> Result<Option<u32>> {
    let bad = || Error::InvalidArgument(format!("{k} must be a post number"));
    match a.get(k) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Number(n)) => n.as_u64().filter(|v| (1..=999_999).contains(v)).map(|v| Some(v as u32)).ok_or_else(bad),
        Some(Value::String(s)) => s.trim().trim_start_matches("P-").parse::<u32>().ok().filter(|v| *v >= 1).map(Some).ok_or_else(bad),
        _ => Err(bad()),
    }
}

fn status_of(fm: &Map) -> &str {
    fm.get("status").and_then(Value::as_str).unwrap_or("open")
}

impl Server {
    /// Resolves `vault:D-004` or `vault:path` to a thread note and its text.
    pub(super) fn resolve_thread(&self, thread: &str) -> Result<(Vault, paths::Resolved, String)> {
        let usage = || Error::InvalidArgument(format!("thread {thread:?}: use vault:D-004 or vault:path"));
        let (vn, rest) = thread.split_once(':').ok_or_else(usage)?;
        let v = self.vault(vn)?;
        let rel = if rest.ends_with(".md") {
            rest.to_string()
        } else {
            let ok = rest.strip_prefix(PREFIX).is_some_and(|d| !d.is_empty() && d.len() <= 9 && d.bytes().all(|c| c.is_ascii_digit()));
            if !ok {
                return Err(usage());
            }
            let dir = paths::resolve_folder(&v, FOLDER).map_err(|_| Error::NotFound(format!("thread {thread}")))?;
            let mut names: Vec<String> = std::fs::read_dir(&dir.full)?
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".md") && (*n == format!("{rest}.md") || n.starts_with(&format!("{rest} "))))
                .collect();
            names.sort();
            let name = names.into_iter().next().ok_or_else(|| Error::NotFound(format!("thread {thread}")))?;
            format!("{}/{name}", dir.rel)
        };
        let res = paths::resolve_existing(&v, &rel)?;
        let text = fsio::read_text(&res.full)?;
        let fm = markdown::parse_frontmatter(&text);
        if !(fm.contains_key("topic") && fm.contains_key("posts")) {
            return Err(Error::InvalidArgument(format!("{} is not a discussion thread", res.rel)));
        }
        Ok((v, res, text))
    }

    fn thread_name(v: &Vault, rel: &str) -> String {
        format!("{}:{}", v.name, thread_id(rel))
    }

    /// The stable link to post `n` of a thread, from the thread's own vault.
    fn post_link(&self, v: &Vault, rel: &str, n: u32) -> Result<String> {
        self.make_link(&v.name, &v.name, rel, Some(&format!("P-{n}")), Some(&format!("{} P-{n}", thread_id(rel))))
    }

    /// Thread rows for indices and listings, with computed keys.
    pub(super) fn thread_rows(&self, vault: Option<&str>) -> Result<Vec<NoteRow>> {
        let mut out = Vec::new();
        for v in self.store.vaults().all() {
            if vault.is_some_and(|x| x != v.name) {
                continue;
            }
            for r in self.store.notes(Some(&v.name), FOLDER, false, Some("D-*.md"))? {
                if is_thread_row(&r) {
                    out.push(self.augment_thread(r));
                }
            }
        }
        Ok(out)
    }

    /// Adds `thread`, `id`, `contested`, `last_post` and `integrity` to a
    /// thread row's frontmatter.
    pub(super) fn augment_thread(&self, mut r: NoteRow) -> NoteRow {
        let text = self.vault(&r.vault).ok().and_then(|v| fsio::read_text(&v.root.join(&r.path)).ok()).unwrap_or_default();
        let t = disc::parse(&text);
        let fm = &mut r.frontmatter;
        fm.insert("thread".into(), json!(format!("{}:{}", r.vault, thread_id(&r.path))));
        fm.insert("id".into(), json!(thread_id(&r.path)));
        fm.insert("contested".into(), json!(disc::contested(&t).len()));
        fm.insert("last_post".into(), json!(t.posts.last().map(|p| p.time.clone()).unwrap_or_default()));
        fm.insert("integrity".into(), json!(disc::verify(&t).map(|_| "ok".to_string()).unwrap_or_else(|e| e.to_string())));
        r
    }

    pub(super) fn open_discussion(&mut self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let topic = one_line(arg_str(a, "topic")?, "topic", 3, 200)?;
        let mut subjects = Vec::new();
        match a.get("subjects") {
            Some(Value::Array(items)) if items.len() <= 20 => {
                for it in items {
                    let o = it.as_object().ok_or_else(|| Error::InvalidArgument("each subject is {vault, path, heading}".into()))?;
                    subjects.push(self.make_link(&v.name, arg_str(o, "vault")?, arg_str(o, "path")?, opt_str(o, "heading")?, None)?);
                }
            }
            _ => return Err(Error::InvalidArgument("subjects must be an array of at most 20 {vault, path, heading}".into())),
        }
        let mut participants: Vec<String> = Vec::new();
        for p in str_list(a, "participants", 16)? {
            let p = disc::check_author(&p)?;
            if !participants.iter().any(|x| x.eq_ignore_ascii_case(&p)) {
                participants.push(p);
            }
        }
        let op =
            a.get("opening_post").and_then(Value::as_object).ok_or_else(|| Error::InvalidArgument("opening_post is required".into()))?;
        let author = disc::check_author(arg_str(op, "author")?)?;
        let kind = opt_str(op, "kind")?.unwrap_or("claim");
        if kind != "claim" && kind != "question" {
            return Err(Error::InvalidArgument("the opening post is a claim or a question".into()));
        }
        let body = disc::check_body(write_text(op, "body")?)?;
        if !participants.iter().any(|x| x.eq_ignore_ascii_case(&author)) {
            participants.push(author.clone());
        }
        let time = disc::utc_now();
        let content = disc::new_thread(&topic, &subjects, &participants, &author, kind, &body, &time)?;
        self.reason = format!("opened discussion: {}", short_text(&topic, 120));
        self.discussion = THIS_NOTE.to_string();
        paths::ensure_folder(&v, FOLDER)?;
        let who = self.actor();
        let slug = disc::slug(&topic);
        let (id, written) = self.store.next_id("open_discussion", &who, &v, FOLDER, PREFIX, 3, Some((&slug, &content)))?;
        let written = written.ok_or_else(|| Error::Io("the thread note was not created".into()))?;
        let link = self.post_link(&v, &written.path, 1)?;
        Ok(json!({"thread": format!("{}:{id}", v.name), "path": written.path, "post": 1, "link": link, "write": wr(&written)}))
    }

    pub(super) fn post(&mut self, a: &Map) -> Result<Value> {
        let (v, res, _) = self.resolve_thread(arg_str(a, "thread")?)?;
        let author = disc::check_author(arg_str(a, "author")?)?;
        let kind = arg_str(a, "kind")?.to_string();
        if matches!(kind.as_str(), "verdict" | "unresolved" | "decision") {
            return Err(Error::InvalidArgument(format!(
                "a {kind} post comes from conclude_discussion, mark_unresolved or record_decision"
            )));
        }
        let body = disc::check_body(write_text(a, "body")?)?;
        let re = opt_post(a, "re")?;
        self.reason = format!("{kind} by {author}: {}", short_text(&body, 100));
        self.discussion = format!("{}:{}", v.name, res.rel);
        let who = self.actor();
        let time = disc::utc_now();
        let mut n = 0;
        let w = self.store.modify("post", &who, &v, &res.rel, None, |text| {
            let t = disc::parse(text);
            let mut extra = Map::new();
            if status_of(&t.frontmatter) == "unresolved" {
                // Posting resumes an unresolved thread.
                extra.insert("status".into(), json!("open"));
                extra.insert("needs_decision".into(), json!(false));
            }
            let (out, m) = disc::append_post(text, &author, &kind, re, &body, &time, &extra, &["open", "parked", "unresolved"])?;
            n = m;
            Ok(out)
        })?;
        let link = self.post_link(&v, &res.rel, n)?;
        Ok(json!({"thread": Self::thread_name(&v, &res.rel), "post": n, "link": link, "write": wr(&w)}))
    }

    pub(super) fn read_discussion(&self, a: &Map) -> Result<Value> {
        let (v, res, text) = self.resolve_thread(arg_str(a, "thread")?)?;
        let since = opt_post(a, "since_post")?.unwrap_or(0);
        let t = disc::parse(&text);
        let integrity = disc::verify(&t).map(|_| "ok".to_string()).unwrap_or_else(|e| e.to_string());
        let posts: Vec<&disc::Post> = t.posts.iter().filter(|p| p.n > since).collect();
        let filter = WriteFilter { discussion: Some(format!("{}:{}", v.name, res.rel)), ..Default::default() };
        let (log, _) = self.store.write_log(&filter, MAX_LIST)?;
        // Outcomes are the writes elsewhere that cite this thread; the thread's own posts are already above.
        let outcomes: Vec<_> = log.into_iter().filter(|e| !(e.vault == v.name && e.path == res.rel)).collect();
        Ok(json!({
            "thread": Self::thread_name(&v, &res.rel), "path": res.rel, "frontmatter": t.frontmatter,
            "integrity": integrity, "post_count": t.posts.len(), "posts": posts, "outcomes": outcomes
        }))
    }

    pub(super) fn discussion_status(&self, a: &Map) -> Result<Value> {
        let (v, res, text) = self.resolve_thread(arg_str(a, "thread")?)?;
        let t = disc::parse(&text);
        let contested: Vec<u32> = disc::contested(&t).iter().map(|p| p.n).collect();
        Ok(json!({
            "thread": Self::thread_name(&v, &res.rel), "status": status_of(&t.frontmatter),
            "points": disc::status(&t), "contested_challenges": contested, "flags": disc::ignored_concedes(&t),
            "integrity": disc::verify(&t).map(|_| "ok".to_string()).unwrap_or_else(|e| e.to_string())
        }))
    }

    pub(super) fn awaiting(&self, a: &Map) -> Result<Value> {
        let author = disc::check_author(arg_str(a, "author")?)?;
        let vault = match opt_str(a, "vault")? {
            Some(x) => Some(self.vault(x)?.name),
            None => None,
        };
        let mut out = Vec::new();
        for r in self.thread_rows(vault.as_deref())? {
            if status_of(&r.frontmatter) == "concluded" {
                continue;
            }
            let v = self.vault(&r.vault)?;
            let text = fsio::read_text(&v.root.join(&r.path))?;
            let t = disc::parse(&text);
            for p in disc::awaiting(&t, &author) {
                out.push(json!({
                    "thread": format!("{}:{}", r.vault, thread_id(&r.path)), "path": r.path,
                    "topic": r.frontmatter.get("topic"), "post": p.n, "author": p.author, "kind": p.kind,
                    "re": p.re, "time": p.time, "body": p.body
                }));
            }
        }
        Ok(json!({"author": author, "awaiting": out}))
    }

    pub(super) fn conclude_discussion(&mut self, a: &Map) -> Result<Value> {
        let (v, res, _) = self.resolve_thread(arg_str(a, "thread")?)?;
        let verdict = disc::check_body(write_text(a, "verdict_body")?)?;
        let author = disc::check_author(opt_str(a, "author")?.unwrap_or("orchestrator"))?;
        let force = match a.get("force_reason") {
            Some(Value::Null) | None => None,
            Some(r) => Some(check_reason(Some(r)).map_err(|e| Error::InvalidArgument(format!("force_reason: {e}")))?),
        };
        self.discussion = format!("{}:{}", v.name, res.rel);
        let who = self.actor();
        let time = disc::utc_now();
        let mut n = 0;
        let mut left: Vec<disc::Point> = Vec::new();
        let w = self.store.modify("conclude_discussion", &who, &v, &res.rel, None, |text| {
            let t = disc::parse(text);
            disc::verify(&t)?;
            let contested = disc::contested(&t);
            let mut body = verdict.clone();
            if !contested.is_empty() {
                let list: Vec<String> = contested
                    .iter()
                    .map(|p| format!("P-{} (challenge by {}{})", p.n, p.author, p.re.map(|r| format!(" to P-{r}")).unwrap_or_default()))
                    .collect();
                let Some(f) = &force else {
                    return Err(Error::InvalidArgument(format!(
                        "{} challenge(s) still contested: {}. Either pass force_reason to conclude anyway (they are listed in the verdict), or call mark_unresolved to hand the disagreement to a human decision",
                        contested.len(),
                        list.join(", ")
                    )));
                };
                body.push_str(&format!("\n\n**Still contested:** {}.\n\n**Concluded over them because:** {f}", list.join(", ")));
            }
            let mut extra = Map::new();
            extra.insert("status".into(), json!("concluded"));
            extra.insert("concluded".into(), json!(time));
            extra.insert("needs_decision".into(), json!(false));
            let (out, m) = disc::append_post(text, &author, "verdict", Some(1), &body, &time, &extra, &["open", "parked"])?;
            n = m;
            left = contested;
            Ok(out)
        })?;
        let link = self.post_link(&v, &res.rel, n)?;
        Ok(json!({"thread": Self::thread_name(&v, &res.rel), "post": n, "verdict_link": link, "still_contested": left, "write": wr(&w)}))
    }

    pub(super) fn mark_unresolved(&mut self, a: &Map) -> Result<Value> {
        let (v, res, _) = self.resolve_thread(arg_str(a, "thread")?)?;
        let summary = disc::check_body(write_text(a, "summary")?)?;
        let questions: Vec<String> =
            str_list(a, "open_questions", 32)?.iter().map(|q| one_line(q, "each open question", 3, 500)).collect::<Result<_>>()?;
        if questions.is_empty() {
            return Err(Error::InvalidArgument("give at least one open question".into()));
        }
        let settle = disc::check_body(write_text(a, "settle_by")?)?;
        let author = disc::check_author(opt_str(a, "author")?.unwrap_or("orchestrator"))?;
        let mut positions: Vec<(String, String, u32)> = Vec::new();
        match a.get("positions") {
            Some(Value::Array(items)) if items.len() <= 16 => {
                for it in items {
                    let o =
                        it.as_object().ok_or_else(|| Error::InvalidArgument("each position is {participant, position, post}".into()))?;
                    let who = disc::check_author(arg_str(o, "participant")?)?;
                    let pos = one_line(arg_str(o, "position")?, "a position", 12, 1000)?;
                    let post = opt_post(o, "post")?.ok_or_else(|| Error::InvalidArgument("each position needs post".into()))?;
                    positions.push((who, pos, post));
                }
            }
            _ => return Err(Error::InvalidArgument("positions must be an array of {participant, position, post}".into())),
        }
        self.discussion = format!("{}:{}", v.name, res.rel);
        let who = self.actor();
        let time = disc::utc_now();
        let target = if self.stem_unique(&v.name, &res.rel)? {
            links::stem(&res.rel).to_string()
        } else {
            res.rel.strip_suffix(".md").unwrap_or(&res.rel).to_string()
        };
        let mut n = 0;
        let w = self.store.modify("mark_unresolved", &who, &v, &res.rel, None, |text| {
            let t = disc::parse(text);
            disc::verify(&t)?;
            // Everyone who has posted needs a final position.
            let mut posters: Vec<&str> = t.posts.iter().map(|p| p.author.as_str()).collect();
            posters.sort_by_key(|p| p.to_lowercase());
            posters.dedup_by(|x, y| x.eq_ignore_ascii_case(y));
            for p in posters {
                if !positions.iter().any(|(w, _, _)| w.eq_ignore_ascii_case(p)) {
                    return Err(Error::InvalidArgument(format!("positions needs an entry for participant {p}")));
                }
            }
            let mut body = format!("**Summary.** {summary}\n\n**Positions.**\n\n");
            for (who, pos, post) in &positions {
                let p = t.posts.iter().find(|q| q.n == *post).ok_or_else(|| Error::InvalidArgument(format!("P-{post} does not exist")))?;
                body.push_str(&format!("- **{who}:** {pos} (strongest: [[{target}#P-{} {} - {}|P-{}]])\n", p.n, p.author, p.kind, p.n));
            }
            body.push_str("\n**Open questions.**\n\n");
            for q in &questions {
                body.push_str(&format!("- {q}\n"));
            }
            body.push_str(&format!("\n**What would settle it.** {settle}"));
            let mut extra = Map::new();
            extra.insert("status".into(), json!("unresolved"));
            extra.insert("needs_decision".into(), json!(true));
            extra.insert("unresolved_at".into(), json!(disc::utc_now_ms()));
            extra.insert("decision_summary".into(), json!(short_text(&summary, 300)));
            extra.insert("open_questions".into(), json!(questions.len()));
            let (out, m) = disc::append_post(text, &author, "unresolved", Some(1), &body, &time, &extra, &["open", "parked"])?;
            n = m;
            Ok(out)
        })?;
        let link = self.post_link(&v, &res.rel, n)?;
        Ok(json!({"thread": Self::thread_name(&v, &res.rel), "post": n, "link": link, "write": wr(&w)}))
    }

    pub(super) fn record_decision(&mut self, a: &Map) -> Result<Value> {
        let (v, res, _) = self.resolve_thread(arg_str(a, "thread")?)?;
        let by = disc::check_author(arg_str(a, "decided_by")?)?;
        let decision = disc::check_body(write_text(a, "decision")?)?;
        self.discussion = format!("{}:{}", v.name, res.rel);
        let who = self.actor();
        let time = disc::utc_now();
        let mut n = 0;
        let w = self.store.modify("record_decision", &who, &v, &res.rel, None, |text| {
            let t = disc::parse(text);
            let re = t.posts.iter().rev().find(|p| p.kind == "unresolved").map(|p| p.n).unwrap_or(1);
            let mut extra = Map::new();
            extra.insert("status".into(), json!("concluded"));
            extra.insert("concluded".into(), json!(time));
            extra.insert("needs_decision".into(), json!(false));
            // A decided thread no longer waits on anything: clear the unresolved markers so
            // the frontmatter does not keep advertising an open queue entry.
            extra.insert("unresolved_at".into(), json!(""));
            extra.insert("open_questions".into(), json!(0));
            extra.insert("decision_summary".into(), json!(""));
            let (out, m) = disc::append_post(text, &by, "decision", Some(re), &decision, &time, &extra, &["unresolved"])?;
            n = m;
            Ok(out)
        })?;
        let link = self.post_link(&v, &res.rel, n)?;
        Ok(json!({"thread": Self::thread_name(&v, &res.rel), "post": n, "link": link, "write": wr(&w)}))
    }

    fn thread_summary(r: &NoteRow) -> Value {
        let fm = &r.frontmatter;
        let g = |k: &str| fm.get(k).cloned().unwrap_or(Value::Null);
        json!({
            "thread": g("thread"), "path": r.path, "topic": g("topic"), "status": g("status"), "participants": g("participants"),
            "subjects": g("subjects"), "posts": g("posts"), "contested": g("contested"),
            "needs_decision": fm.get("needs_decision").cloned().unwrap_or(json!(false)),
            "opened": g("opened"), "concluded": g("concluded"), "last_post": g("last_post"), "integrity": g("integrity")
        })
    }

    pub(super) fn list_discussions(&self, a: &Map) -> Result<Value> {
        let vault = match opt_str(a, "vault")? {
            Some(x) => Some(self.vault(x)?.name),
            None => None,
        };
        let status = opt_str(a, "status")?;
        let subject = opt_str(a, "subject")?.map(str::to_lowercase);
        let needs = match a.get("needs_decision") {
            Some(Value::Bool(b)) => Some(*b),
            Some(Value::Null) | None => None,
            _ => return Err(Error::InvalidArgument("needs_decision must be a boolean".into())),
        };
        let mut out = Vec::new();
        for r in self.thread_rows(vault.as_deref())? {
            let fm = &r.frontmatter;
            if status.is_some_and(|s| status_of(fm) != s) {
                continue;
            }
            if needs.is_some_and(|n| fm.get("needs_decision").and_then(Value::as_bool).unwrap_or(false) != n) {
                continue;
            }
            if let Some(sub) = &subject {
                let hit = match fm.get("subjects") {
                    Some(Value::Array(x)) => {
                        x.iter().filter_map(Value::as_str).any(|s| links::percent_decode(s).to_lowercase().contains(sub.as_str()))
                    }
                    _ => false,
                };
                if !hit {
                    continue;
                }
            }
            out.push(Self::thread_summary(&r));
        }
        Ok(json!({ "discussions": out }))
    }

    pub(super) fn decision_queue(&self, a: &Map) -> Result<Value> {
        // With `repo`: the threads whose file names that repository or one
        // it talks to or is used by (topic, subjects, summary or posts).
        let covered = self.repo_coverage(a)?;
        let mut rows: Vec<NoteRow> = self.thread_rows(None)?.into_iter().filter(|r| status_of(&r.frontmatter) == "unresolved").collect();
        let at = |r: &NoteRow| r.frontmatter.get("unresolved_at").and_then(Value::as_str).unwrap_or("").to_string();
        rows.sort_by(|x, y| at(x).cmp(&at(y)).then_with(|| x.vault.cmp(&y.vault)).then_with(|| x.path.cmp(&y.path)));
        let mut out = Vec::new();
        for r in &rows {
            let v = self.vault(&r.vault)?;
            let text = fsio::read_text(&v.root.join(&r.path))?;
            if covered.as_ref().is_some_and(|c| !crate::project::Graph::mentions(&text, c)) {
                continue;
            }
            let last = disc::parse(&text).posts.iter().rev().find(|p| p.kind == "unresolved").map(|p| p.n);
            let link = match last {
                Some(n) => Some(self.post_link(&v, &r.path, n)?),
                None => None,
            };
            let fm = &r.frontmatter;
            out.push(json!({
                "thread": fm.get("thread"), "path": r.path, "topic": fm.get("topic"), "unresolved_at": fm.get("unresolved_at"),
                "summary": fm.get("decision_summary"), "open_questions": fm.get("open_questions"),
                "participants": fm.get("participants"), "link": link
            }));
        }
        Ok(json!({ "queue": out }))
    }
}
