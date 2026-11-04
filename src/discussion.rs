//! Agent discussions: one note per thread, an append-only list of posts.
//!
//! A thread note looks like this:
//!
//! ```text
//! ---
//! topic: "Present thread off the frame thread"
//! subjects: ["[[Ideas/PI-010 Present thread off the frame thread]]"]
//! participants: [bob, alice]
//! status: "open"
//! opened: "2026-10-08T14:30:00Z"
//! concluded: ""
//! posts: 2
//! chain: "1f0c..."
//! tags: [discussion]
//! ---
//!
//! # D-001 Present thread off the frame thread
//!
//! ## Subjects
//!
//! - [[Ideas/PI-010 Present thread off the frame thread]]
//!
//! ## Posts
//!
//! ### P-1 bob - claim
//!
//! *2026-10-08T14:30:00Z | sha: 0a1b2c3d4e5f6a7b*
//!
//! The present thread saves 0.8 ms at 4K.
//!
//! ### P-2 alice - challenge
//!
//! *2026-10-08T14:31:00Z | re: P-1 | sha: ...*
//!
//! Only at GPU-bound scenes.
//! ```
//!
//! Posts are immutable. Each post's metadata line carries the hash of the
//! post itself, and the frontmatter carries the post count and a hash chain
//! over all posts, so a hand edit to any earlier post, a removed post or text
//! slipped in after the last post is caught on the next append and reported
//! by post number.

use std::collections::HashMap;

use crate::json::{Map, Value};
use crate::lex;

use crate::error::{Error, Result};
use crate::markdown;
use crate::sha256;

/// The folder threads live in, in every vault.
pub const FOLDER: &str = "Discussions";
/// The id prefix of threads (`D-001`).
pub const PREFIX: &str = "D-";

/// Every post kind.
pub const KINDS: [&str; 11] =
    ["claim", "evidence", "challenge", "answer", "concede", "retract", "question", "proposal", "verdict", "unresolved", "decision"];

/// The shortest post body accepted, in characters after trimming.
pub const MIN_BODY: usize = 12;

/// One parsed post.
#[derive(Debug, Clone)]
pub struct Post {
    /// Post number, from 1.
    pub n: u32,
    /// Free-text author (a persona name).
    pub author: String,
    /// One of [`KINDS`].
    pub kind: String,
    /// UTC time it was posted.
    pub time: String,
    /// The post it replies to.
    pub re: Option<u32>,
    /// The body text.
    pub body: String,
    /// The hash recorded in the metadata line.
    pub sha: String,
    /// Whether the recorded hash matches the post as it is now.
    pub intact: bool,
}

crate::impl_to_json!(Post { n, author, kind, time, re, body, sha, intact });

/// A parsed thread.
#[derive(Debug, Clone)]
pub struct Thread {
    /// The frontmatter.
    pub frontmatter: Map,
    /// Posts in file order.
    pub posts: Vec<Post>,
    /// Byte offset where the next post goes (the end of the note).
    pub end: usize,
}

/// A post heading, `### P-N author - kind` (was
/// `^### P-(\d{1,6}) (.+) - ([a-z]+)\s*$`): groups 0, the number, the
/// author (the longest that leaves ` - kind`) and the kind.
pub(crate) fn post_heading(s: &str) -> Option<[lex::Span; 4]> {
    let n0 = lex::lit(s, 0, "### P-")?;
    let (n1, count) = lex::run_max(s, n0, 7, lex::digit);
    if !(1..=6).contains(&count) {
        return None;
    }
    let a0 = lex::lit(s, n1, " ")?;
    // The kind is the run of `a-z` that ends the line before trailing space.
    let t = s.trim_end_matches(lex::space).len();
    let k0 = s[..t].char_indices().rev().find(|&(_, c)| !c.is_ascii_lowercase()).map_or(0, |(i, c)| i + c.len_utf8());
    if k0 >= t || k0 < a0 + 4 || &s.as_bytes()[k0 - 3..k0] != b" - " || s[a0..k0 - 3].contains('\n') {
        return None;
    }
    Some([Some((0, s.len())), Some((n0, n1)), Some((a0, k0 - 3)), Some((k0, t))])
}

/// A post's metadata line, `*TIME | re: P-N | sha: HASH*` (was
/// `^\*(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)(?: \| re: P-(\d{1,6}))? \| sha: ([0-9a-f]{16})\*\s*$`):
/// groups 0, the time, the `re` number and the hash.
pub(crate) fn post_meta(s: &str) -> Option<[lex::Span; 4]> {
    let t0 = lex::lit(s, 0, "*")?;
    let mut i = t0;
    for (n, sep) in [(4, "-"), (2, "-"), (2, "T"), (2, ":"), (2, ":"), (2, "Z")] {
        let (j, got) = lex::run_max(s, i, n, lex::digit);
        if got != n {
            return None;
        }
        i = lex::lit(s, j, sep)?;
    }
    let time = (t0, i);
    let mut re = None;
    if let Some(r0) = lex::lit(s, i, " | re: P-") {
        let (r1, count) = lex::run_max(s, r0, 7, lex::digit);
        if !(1..=6).contains(&count) {
            return None;
        }
        re = Some((r0, r1));
        i = r1;
    }
    let h0 = lex::lit(s, i, " | sha: ")?;
    let (h1, count) = lex::run_max(s, h0, 16, |c| matches!(c, '0'..='9' | 'a'..='f'));
    if count != 16 {
        return None;
    }
    let e = lex::lit(s, h1, "*")?;
    (lex::spaces(s, e) == s.len()).then_some([Some((0, s.len())), Some(time), re, Some((h0, h1))])
}

#[cfg(test)]
pub(crate) fn lex_sites() -> Vec<lex::props::Site> {
    use lex::props::{v, Mode, Site};
    vec![
        Site {
            name: "discussion heading",
            pattern: r"^### P-(\d{1,6}) (.+) - ([a-z]+)\s*$".into(),
            tokens: &[
                "### P-", "### P-1 ", "1", "123456", "1234567", "\u{663}", " - ", " -", "- ", "claim", "a", "Z", " ", "\t", "\n", "alice",
            ],
            examples: &["### P-12 alice - claim", "### P-3 bob - x - y - reply  ", "### P-123456 a b - c\t"],
            mode: Mode::One(|s| post_heading(s).map(v)),
        },
        Site {
            name: "discussion meta",
            pattern: r"^\*(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)(?: \| re: P-(\d{1,6}))? \| sha: ([0-9a-f]{16})\*\s*$".into(),
            tokens: &[
                "*",
                "2026-01-01T00:00:00Z",
                "2026",
                "-",
                "T",
                ":",
                "Z",
                "00",
                "\u{663}",
                " | re: P-",
                "12",
                "1234567",
                " | sha: ",
                "0123456789abcdef",
                "f",
                "*",
                " ",
                "\n",
            ],
            examples: &["*2026-01-01T00:00:00Z | sha: 0123456789abcdef*", "*2026-11-02T10:20:30Z | re: P-4 | sha: ffffffffffffffff* "],
            mode: Mode::One(|s| post_meta(s).map(v)),
        },
    ]
}

/// The hash of one post: its number, author, kind, time, `re` and body.
pub fn post_sha(n: u32, author: &str, kind: &str, time: &str, re: Option<u32>, body: &str) -> String {
    let re = re.map(|r| r.to_string()).unwrap_or_default();
    let canon = format!("P-{n}\n{author}\n{kind}\n{time}\n{re}\n{}", body.replace("\r\n", "\n"));
    sha256::hex(canon.as_bytes())[..16].to_string()
}

/// The next link of the chain over all posts.
pub fn chain(prev: &str, sha: &str) -> String {
    sha256::hex(format!("{prev}{sha}").as_bytes())[..16].to_string()
}

/// Parses a thread note.
pub fn parse(text: &str) -> Thread {
    let frontmatter = markdown::parse_frontmatter(text);
    let start = markdown::body_start(text);
    // Line spans of the body.
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut pos = start;
    for l in text[start..].split_inclusive('\n') {
        lines.push((pos, l.trim_end_matches(['\n', '\r'])));
        pos += l.len();
    }
    let heads: Vec<(usize, [lex::Span; 4])> = lines.iter().enumerate().filter_map(|(i, (_, l))| post_heading(l).map(|c| (i, c))).collect();
    let mut posts = Vec::new();
    for (k, &(i, c)) in heads.iter().enumerate() {
        let l = lines[i].1;
        let g = |n: usize| c[n].map_or("", |(a, b)| &l[a..b]);
        let n: u32 = g(1).parse().unwrap_or(0);
        let author = g(2).trim().to_string();
        let kind = g(3).to_string();
        let stop = heads.get(k + 1).map_or(lines.len(), |h| h.0);
        // The metadata line is the first non-blank line after the heading.
        let mut j = i + 1;
        while j < stop && lines[j].1.trim().is_empty() {
            j += 1;
        }
        let meta = lines.get(j).filter(|_| j < stop).and_then(|&(_, l)| post_meta(l).map(|m| (l, m)));
        let (time, re, sha, body_from) = match meta {
            Some((l, m)) => {
                let g = |n: usize| m[n].map(|(a, b)| &l[a..b]);
                (g(1).unwrap_or("").to_string(), g(2).and_then(|x| x.parse().ok()), g(3).unwrap_or("").to_string(), j + 1)
            }
            None => (String::new(), None, String::new(), i + 1),
        };
        let body_lines: Vec<&str> = lines[body_from.min(stop)..stop].iter().map(|(_, l)| *l).collect();
        let body = body_lines.join("\n").trim_matches('\n').trim_end().to_string();
        let body = body.trim_start_matches('\n').to_string();
        let intact = !sha.is_empty() && post_sha(n, &author, &kind, &time, re, &body) == sha;
        posts.push(Post { n, author, kind, time, re, body, sha, intact });
    }
    Thread { frontmatter, posts, end: text.len() }
}

/// Checks that no earlier post was changed, removed or added by hand.
///
/// # Errors
///
/// [`Error::InvalidArgument`] naming the first post whose hash no longer
/// matches, a gap or repeat in the numbering, or a post count or chain that
/// disagrees with the frontmatter (a post removed or added by hand).
pub fn verify(t: &Thread) -> Result<()> {
    let mut prev = String::new();
    for (i, p) in t.posts.iter().enumerate() {
        if p.n as usize != i + 1 {
            return Err(Error::InvalidArgument(format!(
                "thread integrity: expected P-{} but found P-{}; a post was removed, reordered or added by hand",
                i + 1,
                p.n
            )));
        }
        if !p.intact {
            return Err(Error::InvalidArgument(format!(
                "thread integrity: P-{} ({} - {}) was edited by hand after it was posted; posts are immutable, post a retract instead",
                p.n, p.author, p.kind
            )));
        }
        prev = chain(&prev, &p.sha);
    }
    let count = t.frontmatter.get("posts").and_then(Value::as_u64).unwrap_or(0) as usize;
    if count != t.posts.len() {
        return Err(Error::InvalidArgument(format!(
            "thread integrity: the note has {} posts but its frontmatter records {count}; a post was removed or added by hand",
            t.posts.len()
        )));
    }
    let recorded = t.frontmatter.get("chain").and_then(Value::as_str).unwrap_or("");
    if recorded != prev {
        return Err(Error::InvalidArgument("thread integrity: the post chain does not match the frontmatter".into()));
    }
    Ok(())
}

/// Checks a post kind and its `re`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an unknown kind, a missing `re` on a kind
/// other than claim or question, or a `re` that names no earlier post.
pub fn check_kind(kind: &str, re: Option<u32>, next: u32) -> Result<()> {
    if !KINDS.contains(&kind) {
        return Err(Error::InvalidArgument(format!("kind {kind:?}; use one of {}", KINDS.join(", "))));
    }
    if re.is_none() && kind != "claim" && kind != "question" {
        return Err(Error::InvalidArgument(format!("a {kind} must reply to a post: give re")));
    }
    if let Some(r) = re {
        if r == 0 || r >= next {
            return Err(Error::InvalidArgument(format!("re: P-{r} is not an earlier post in this thread")));
        }
    }
    Ok(())
}

/// Checks a post body.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if it is shorter than [`MIN_BODY`] characters
/// after trimming, or has a line that would read as a level 1 to 3 heading
/// (which would split the thread's structure).
pub fn check_body(body: &str) -> Result<String> {
    let b = body.replace("\r\n", "\n");
    let b = b.trim_matches('\n').trim_end();
    if b.trim().chars().count() < MIN_BODY {
        return Err(Error::InvalidArgument(format!("a post body needs at least {MIN_BODY} characters")));
    }
    for l in b.lines() {
        let t = l.trim_start();
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=3).contains(&hashes) && t[hashes..].starts_with([' ', '\t']) {
            return Err(Error::InvalidArgument(format!("post bodies may not contain level 1-3 headings ({l:?}); use #### or bold text")));
        }
        if post_meta(l).is_some() {
            return Err(Error::InvalidArgument("post bodies may not contain a post metadata line".into()));
        }
    }
    Ok(b.to_string())
}

/// Checks an author name: 1 to 64 characters on one line, without `#`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] otherwise.
pub fn check_author(a: &str) -> Result<String> {
    let a = a.trim();
    if a.is_empty() || a.chars().count() > 64 || a.contains(|c: char| c.is_control() || c == '#') || a.contains(" - ") {
        return Err(Error::InvalidArgument("author must be 1 to 64 characters on one line, without '#' or ' - '".into()));
    }
    Ok(a.to_string())
}

/// Renders one post block (with a leading blank line) and returns its hash.
pub fn render_post(n: u32, author: &str, kind: &str, time: &str, re: Option<u32>, body: &str, nl: &str) -> (String, String) {
    let sha = post_sha(n, author, kind, time, re, body);
    let re_part = re.map(|r| format!(" | re: P-{r}")).unwrap_or_default();
    let body = markdown::to_eol(body, nl);
    (format!("{nl}### P-{n} {author} - {kind}{nl}{nl}*{time}{re_part} | sha: {sha}*{nl}{nl}{body}{nl}"), sha)
}

/// A file-name slug for a topic: letters, digits, spaces and `-`, at most 60
/// characters.
pub fn slug(topic: &str) -> String {
    let s: String = topic.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { ' ' }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s: String = s.chars().take(60).collect();
    let s = s.trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() {
        "thread".to_string()
    } else {
        s
    }
}

/// The current UTC time, `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    civil(secs)
}

/// The current UTC time with milliseconds, `YYYY-MM-DDTHH:MM:SS.mmmZ`, for
/// orderings finer than a second (the decision queue).
pub fn utc_now_ms() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let s = civil(d.as_secs() as i64);
    format!("{}.{:03}Z", &s[..s.len() - 1], d.subsec_millis())
}

pub(crate) fn civil(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil from days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Where a claim, challenge or question stands.
#[derive(Debug, Clone)]
pub struct Point {
    /// The post number.
    pub n: u32,
    /// Its kind: claim, challenge or question.
    pub kind: String,
    /// Its author.
    pub author: String,
    /// The post it replies to, if any.
    pub re: Option<u32>,
    /// `answered`, `conceded`, `retracted`, `contested`, `unchallenged`
    /// (a claim nobody challenged) or `open` (an unanswered question).
    pub state: String,
    /// The replies that decided the state.
    pub by: Vec<u32>,
}

crate::impl_to_json!(Point { n, kind, author, re, state, by });

/// True if a `concede` post decides the point it replies to: its author must
/// be the challenged author, that is the author of the post the conceded
/// post replies to (or of the conceded post itself when that replies to
/// nothing). Any other concede stays in the thread as an ordinary reply.
pub fn concede_counts(c: &Post, by_n: &HashMap<u32, &Post>) -> bool {
    let Some(x) = c.re.and_then(|r| by_n.get(&r)) else { return false };
    let challenged = x.re.and_then(|r| by_n.get(&r)).unwrap_or(x);
    c.author.eq_ignore_ascii_case(&challenged.author)
}

/// Concede posts that do not decide their point, as
/// `{post, concede_ignored: "not the challenged author"}`.
pub fn ignored_concedes(t: &Thread) -> Vec<crate::json::Value> {
    let by_n: HashMap<u32, &Post> = t.posts.iter().map(|p| (p.n, p)).collect();
    t.posts
        .iter()
        .filter(|p| p.kind == "concede" && !concede_counts(p, &by_n))
        .map(|p| crate::json!({"post": p.n, "author": p.author, "re": p.re, "concede_ignored": "not the challenged author"}))
        .collect()
}

/// Derives the state of every claim, challenge and question from the `re`
/// chains.
///
/// Rules, applied to the direct replies of a post X:
/// * a `retract` reply makes X `retracted`; a `concede` reply makes it
///   `conceded`, but only when its author wrote the post X challenges (or X
///   itself, when X replies to nothing); see [`concede_counts`];
/// * an `answer` or `evidence` reply stands unless one of its own challenges
///   is conceded or still contested;
/// * a challenge with a standing answer is `answered`, otherwise `contested`;
/// * a claim is `conceded` if any challenge to it was conceded, `contested`
///   if any is still contested, `answered` if every challenge was answered
///   or retracted, and `unchallenged` if it has none;
/// * a question is `answered` once it has a standing answer, else `open`.
pub fn status(t: &Thread) -> Vec<Point> {
    let mut replies: HashMap<u32, Vec<&Post>> = HashMap::new();
    for p in &t.posts {
        if let Some(r) = p.re {
            replies.entry(r).or_default().push(p);
        }
    }
    let by_n: HashMap<u32, &Post> = t.posts.iter().map(|p| (p.n, p)).collect();
    let mut memo: HashMap<u32, (String, Vec<u32>)> = HashMap::new();
    fn state(
        n: u32,
        by_n: &HashMap<u32, &Post>,
        replies: &HashMap<u32, Vec<&Post>>,
        memo: &mut HashMap<u32, (String, Vec<u32>)>,
    ) -> (String, Vec<u32>) {
        if let Some(s) = memo.get(&n) {
            return s.clone();
        }
        let empty = Vec::new();
        let rs = replies.get(&n).unwrap_or(&empty);
        let kind = by_n.get(&n).map(|p| p.kind.as_str()).unwrap_or("");
        let pick = |k: &str| rs.iter().filter(|p| p.kind == k).map(|p| p.n).collect::<Vec<_>>();
        let concedes: Vec<u32> = rs.iter().filter(|p| p.kind == "concede" && concede_counts(p, by_n)).map(|p| p.n).collect();
        let out = if !pick("retract").is_empty() {
            ("retracted".to_string(), pick("retract"))
        } else if !concedes.is_empty() {
            ("conceded".to_string(), concedes)
        } else {
            let mut standing = Vec::new();
            for a in rs.iter().filter(|p| p.kind == "answer" || p.kind == "evidence") {
                let ok = replies
                    .get(&a.n)
                    .map(|v| {
                        v.iter().filter(|c| c.kind == "challenge").all(|c| {
                            let s = state(c.n, by_n, replies, memo).0;
                            s == "answered" || s == "retracted"
                        })
                    })
                    .unwrap_or(true);
                if ok {
                    standing.push(a.n);
                }
            }
            match kind {
                "challenge" => {
                    if standing.is_empty() {
                        ("contested".to_string(), Vec::new())
                    } else {
                        ("answered".to_string(), standing)
                    }
                }
                "question" => {
                    if standing.is_empty() {
                        ("open".to_string(), Vec::new())
                    } else {
                        ("answered".to_string(), standing)
                    }
                }
                _ => {
                    let challenges: Vec<u32> = rs.iter().filter(|p| p.kind == "challenge").map(|p| p.n).collect();
                    if challenges.is_empty() {
                        if standing.is_empty() {
                            ("unchallenged".to_string(), Vec::new())
                        } else {
                            ("answered".to_string(), standing)
                        }
                    } else {
                        let states: Vec<(u32, String)> = challenges.iter().map(|c| (*c, state(*c, by_n, replies, memo).0)).collect();
                        let with = |s: &str| states.iter().filter(|(_, x)| x == s).map(|(c, _)| *c).collect::<Vec<_>>();
                        if !with("conceded").is_empty() {
                            ("conceded".to_string(), with("conceded"))
                        } else if !with("contested").is_empty() {
                            ("contested".to_string(), with("contested"))
                        } else {
                            ("answered".to_string(), challenges)
                        }
                    }
                }
            }
        };
        memo.insert(n, out.clone());
        out
    }
    t.posts
        .iter()
        .filter(|p| matches!(p.kind.as_str(), "claim" | "challenge" | "question"))
        .map(|p| {
            let (s, by) = state(p.n, &by_n, &replies, &mut memo);
            Point { n: p.n, kind: p.kind.clone(), author: p.author.clone(), re: p.re, state: s, by }
        })
        .collect()
}

/// Challenges still contested: what a verdict must name, and what blocks an
/// unforced conclude.
pub fn contested(t: &Thread) -> Vec<Point> {
    status(t).into_iter().filter(|p| p.kind == "challenge" && p.state == "contested").collect()
}

/// Posts that challenge or question `author` (they reply to one of the
/// author's posts) with no reply from the author yet.
pub fn awaiting<'a>(t: &'a Thread, author: &str) -> Vec<&'a Post> {
    let mine = |p: &Post| p.author.eq_ignore_ascii_case(author);
    let by_n: HashMap<u32, &Post> = t.posts.iter().map(|p| (p.n, p)).collect();
    t.posts
        .iter()
        .filter(|p| matches!(p.kind.as_str(), "challenge" | "question"))
        .filter(|p| !mine(p))
        .filter(|p| p.re.and_then(|r| by_n.get(&r)).is_some_and(|q| mine(q)))
        .filter(|p| !t.posts.iter().any(|r| r.re == Some(p.n) && mine(r)))
        .collect()
}

/// Builds a new thread note (with `{{id}}` where the id goes).
#[allow(clippy::too_many_arguments)]
pub fn new_thread(
    topic: &str,
    subjects: &[String],
    participants: &[String],
    author: &str,
    kind: &str,
    body: &str,
    time: &str,
) -> Result<String> {
    let (block, sha) = render_post(1, author, kind, time, None, body, "\n");
    let mut fm = Map::new();
    fm.insert("topic".into(), Value::String(topic.to_string()));
    fm.insert("subjects".into(), Value::Array(subjects.iter().cloned().map(Value::String).collect()));
    fm.insert("participants".into(), Value::Array(participants.iter().cloned().map(Value::String).collect()));
    fm.insert("status".into(), Value::String("open".into()));
    fm.insert("opened".into(), Value::String(time.to_string()));
    fm.insert("concluded".into(), Value::String(String::new()));
    fm.insert("posts".into(), Value::from(1));
    fm.insert("chain".into(), Value::String(chain("", &sha)));
    fm.insert("tags".into(), Value::Array(vec![Value::String("discussion".into())]));
    let mut out = String::from("---\n");
    for k in ["topic", "subjects", "participants", "status", "opened", "concluded", "posts", "chain", "tags"] {
        out.push_str(&format!("{k}: {}\n", markdown::render_yaml(&fm[k])?));
    }
    out.push_str("---\n\n");
    out.push_str(&format!("# {{{{id}}}} {}\n\n## Subjects\n\n", topic.replace(['\n', '\r'], " ")));
    for s in subjects {
        out.push_str(&format!("- {s}\n"));
    }
    out.push_str("\n## Posts\n");
    out.push_str(&block);
    Ok(out)
}

/// Appends a post to a verified thread and updates `posts`, `chain` and,
/// for a new author, `participants`.
///
/// # Returns
///
/// The new text and the post number.
///
/// # Arguments
///
/// * `text` - the thread note.
/// * `author`, `kind`, `re`, `body`, `time` - the post.
/// * `extra` - further frontmatter keys to set in the same write (status
///   changes).
/// * `allowed` - the thread statuses this append is valid in.
///
/// # Errors
///
/// Integrity errors from [`verify`], a status not in `allowed`, kind and
/// `re` errors from [`check_kind`], and frontmatter errors.
#[allow(clippy::too_many_arguments)]
pub fn append_post(
    text: &str,
    author: &str,
    kind: &str,
    re: Option<u32>,
    body: &str,
    time: &str,
    extra: &Map,
    allowed: &[&str],
) -> Result<(String, u32)> {
    let t = parse(text);
    verify(&t)?;
    let status = t.frontmatter.get("status").and_then(Value::as_str).unwrap_or("open");
    if !allowed.contains(&status) {
        let hint = match status {
            "concluded" => "; open a new discussion to continue",
            "unresolved" => "; post to resume it, or record_decision to settle it",
            _ => "",
        };
        return Err(Error::InvalidArgument(format!("the thread is {status}; this needs it to be {}{hint}", allowed.join(" or "))));
    }
    let n = t.posts.len() as u32 + 1;
    check_kind(kind, re, n)?;
    let nl = markdown::eol(text);
    let (block, sha) = render_post(n, author, kind, time, re, body, nl);
    let prev = t.frontmatter.get("chain").and_then(Value::as_str).unwrap_or("").to_string();
    let mut updates = extra.clone();
    updates.insert("posts".into(), Value::from(n));
    updates.insert("chain".into(), Value::String(chain(&prev, &sha)));
    let mut participants: Vec<String> = match t.frontmatter.get("participants") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    };
    if !participants.iter().any(|p| p.eq_ignore_ascii_case(author)) {
        participants.push(author.to_string());
        updates.insert("participants".into(), Value::Array(participants.into_iter().map(Value::String).collect()));
    }
    let mut out = markdown::set_frontmatter(text, &updates)?;
    if !out.ends_with('\n') {
        out.push_str(nl);
    }
    out.push_str(&block);
    Ok((out, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(posts: &[(&str, &str, Option<u32>)]) -> String {
        let mut t = new_thread("T", &[], &["a".into()], posts[0].0, posts[0].1, "opening body text", "2026-10-08T00:00:00Z").unwrap();
        for (a, k, re) in &posts[1..] {
            t = append_post(&t, a, k, *re, "a body long enough", "2026-10-08T00:00:01Z", &Map::new(), &["open"]).unwrap().0;
        }
        t
    }

    #[test]
    fn round_trip_and_status() {
        let t = thread(&[
            ("bob", "claim", None),
            ("alice", "challenge", Some(1)),
            ("bob", "answer", Some(2)),
            ("alice", "challenge", Some(1)),
            ("bob", "claim", None),
            ("alice", "challenge", Some(5)),
            ("bob", "concede", Some(6)),
            ("alice", "question", Some(1)),
        ]);
        let p = parse(&t);
        verify(&p).unwrap();
        assert_eq!(p.posts.len(), 8);
        let s: Vec<(u32, String)> = status(&p).into_iter().map(|x| (x.n, x.state)).collect();
        assert_eq!(
            s,
            vec![
                (1, "contested".into()),
                (2, "answered".into()),
                (4, "contested".into()),
                (5, "conceded".into()),
                (6, "conceded".into()),
                (8, "open".into())
            ]
        );
        let w: Vec<u32> = awaiting(&p, "Bob").iter().map(|x| x.n).collect();
        assert_eq!(w, vec![4, 8]);
        assert_eq!(contested(&p).iter().map(|x| x.n).collect::<Vec<_>>(), vec![4]);
    }

    #[test]
    fn answer_that_is_successfully_challenged_does_not_stand() {
        let t = thread(&[
            ("a", "claim", None),
            ("b", "challenge", Some(1)),
            ("a", "answer", Some(2)),
            ("b", "challenge", Some(3)),
            ("a", "concede", Some(4)),
        ]);
        let s: Vec<(u32, String)> = status(&parse(&t)).into_iter().map(|x| (x.n, x.state)).collect();
        assert_eq!(s, vec![(1, "contested".into()), (2, "contested".into()), (4, "conceded".into())]);
    }

    #[test]
    fn utc_now_shape() {
        let t = utc_now();
        assert_eq!(t.len(), 20);
        assert!(t.starts_with("20") && t.ends_with('Z'));
    }
}
