//! Recall: records as compact text, the relevance gate, and the lane view.
//!
//! The same functions answer the `recall` tool and the recall hook, so what a
//! hook injects is exactly what the tool would show.

use std::collections::HashSet;

use crate::error::Result;

use super::vectors::RecVecs;
use super::{Head, MemoryStore};
use crate::embed::gemm::Isa;

/// Words never searched for: they match nearly every record.
const STOP: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "had", "her", "was", "one", "our", "out", "has", "have", "this", "that", "with",
    "from", "they", "will", "would", "there", "their", "what", "about", "which", "when", "make", "like", "than", "then", "them", "these", "some", "into",
    "its", "also", "just", "only", "each", "other", "more", "most", "such", "should", "could", "does", "did", "done", "been", "being", "were", "who",
    "how", "why", "use", "used", "using", "your", "yours", "here", "where", "after", "before", "over", "under", "again", "very", "both", "same", "own",
    "now", "new", "get", "got", "let", "may", "must", "need", "via", "per", "yes", "it's", "don't", "i'm", "please", "thanks", "okay",
];

/// The words of a question worth searching for: lowercase, at least three
/// characters, not a stop word, each once, at most 48.
pub fn terms(text: &str) -> Vec<String> {
    scan_terms(text, false).0
}

/// [`terms`], and how many more distinct words the query had past the 48.
pub fn terms_and_rest(text: &str) -> (Vec<String>, usize) {
    scan_terms(text, true)
}

fn scan_terms(text: &str, count_rest: bool) -> (Vec<String>, usize) {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut rest = 0;
    for w in text.split(|c: char| !c.is_alphanumeric()) {
        if w.chars().count() < 3 || w.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let l = w.to_lowercase();
        if STOP.contains(&l.as_str()) || !seen.insert(l.clone()) {
            continue;
        }
        if out.len() < super::FILTERED_TERMS {
            out.push(l);
        } else if count_rest {
            rest += 1;
        } else {
            break;
        }
    }
    (out, rest)
}

/// One record as text: a header line, then the body (a memory's body only,
/// not its frontmatter), ending with a line break. Written by [`frame`].
pub fn render(out: &mut String, h: &Head) {
    let titled = h.kind == "memory" || h.kind == "journal" || h.kind == "handoff" || h.kind == "decision";
    frame(out, &header(h), titled.then(|| h.title.trim_end()), &h.body);
}

/// A record's header line, as [`render`] frames it.
pub fn header(h: &Head) -> String {
    let mut head = String::with_capacity(h.key.len() + 48);
    head.push_str(&h.key);
    head.push(' ');
    head.push_str(&h.kind);
    if !h.sub.is_empty() {
        head.push('/');
        head.push_str(&h.sub);
    }
    if !h.scope.is_empty() && h.kind != "rule" && h.kind != "section" && h.kind != "block" {
        head.push_str(" lane=");
        head.push_str(&h.scope);
    }
    head.push(' ');
    head.push_str(&h.status);
    if let Some(by) = &h.superseded_by {
        head.push_str(" by ");
        head.push_str(by);
    }
    head.push_str(&format!(" v{} {}", h.seq, h.time.get(..16).unwrap_or(&h.time)));
    if let Some(a) = anchor(h) {
        head.push_str(" @");
        head.push_str(&a);
    }
    head
}

/// Where a record's text sits in its source file, when that says more than
/// its key: `path:line`, or `L<line>` when the key already names the file
/// (`intent:r00042` from `INTENT.md`). A memory's file is its key, so it has
/// none.
pub fn anchor(h: &Head) -> Option<String> {
    let a = h.anchor.as_deref()?;
    let (path, line) = a.rsplit_once(':')?;
    let stem = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md");
    if stem == h.key {
        return None;
    }
    let prefix = h.key.split_once(':').map_or("", |p| p.0);
    Some(if !prefix.is_empty() && stem.eq_ignore_ascii_case(prefix) { format!("L{line}") } else { a.to_string() })
}

/// A pointer's anchor, `src:path:line`: the vault or source, file and line
/// a record's text starts at, so `read_note {vault: src, path, lines}` reads
/// just that part. None when [`anchor`] has none, or the source is unknown.
pub fn pointer_anchor(h: &Head) -> Option<String> {
    anchor(h)?;
    Some(format!("{}:{}", h.anchor_src.as_deref()?, h.anchor.as_deref()?))
}

/// The record header's marker: a line that starts with it opens a record.
pub const HEADER: &str = "== ";

/// Writes one record's framing, the only writer of it: [`HEADER`] and
/// `head` on one line (a line break inside `head` becomes a space), then the
/// title line when there is one, then the body with its outer blank lines
/// trimmed, each ending with a line break. A title or body line that starts
/// with [`HEADER`] (after a `\n`, `\r`, NEL, LS or PS, or at the start) is
/// indented by one space, so record text can never read as a header of its
/// own. Text with no such line is written byte for byte.
pub fn frame(out: &mut String, head: &str, title: Option<&str>, body: &str) {
    out.push_str(HEADER);
    if head.contains(is_break) {
        out.extend(head.chars().map(|c| if is_break(c) { ' ' } else { c }));
    } else {
        out.push_str(head);
    }
    out.push('\n');
    // A title made from the body (a decision's, by default) repeats the
    // body's opening: it is left out rather than shown twice.
    if let Some(t) = title.filter(|t| !t.trim().is_empty() && !opens_with(body, t)) {
        push_text(out, t);
        out.push('\n');
    }
    let body = body.trim_matches('\n');
    if !body.is_empty() {
        push_text(out, body);
        out.push('\n');
    }
}

/// Whether `body`, its runs of whitespace taken as one space, starts with
/// `title` (likewise).
fn opens_with(body: &str, title: &str) -> bool {
    let mut b = body.split_whitespace().flat_map(|w| w.chars().chain(std::iter::once(' ')));
    let mut t = title.split_whitespace().flat_map(|w| w.chars().chain(std::iter::once(' '))).peekable();
    while let Some(c) = t.next() {
        // The title's own last space need not be matched.
        if t.peek().is_none() && c == ' ' {
            return true;
        }
        if b.next() != Some(c) {
            return false;
        }
    }
    true
}

/// A character a reader may take as the end of a line.
fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `text`, with each line that starts with [`HEADER`] indented one space.
fn push_text(out: &mut String, text: &str) {
    if !text.contains(HEADER) {
        out.push_str(text);
        return;
    }
    let mut from = 0;
    let mut line_start = true;
    for (i, c) in text.char_indices() {
        if line_start && text[i..].starts_with(HEADER) {
            out.push_str(&text[from..i]);
            out.push(' ');
            from = i;
        }
        line_start = is_break(c);
    }
    out.push_str(&text[from..]);
}

/// What a gated recall chose.
#[derive(Debug, Clone, Default)]
pub struct Chosen {
    /// The records injected, best first.
    pub heads: Vec<Head>,
    /// Records that cleared the gate but did not fit the budget.
    pub more: usize,
    /// The text.
    pub text: String,
}

/// The relevance gate's floors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gate {
    /// The negated bm25 a lexical match needs.
    pub fts_floor: f64,
    /// The least number of distinct query words a lexical match contains
    /// (fewer when the query has fewer).
    pub min_terms: usize,
    /// The cosine a lexical match also needs, when the record has a vector.
    pub cos_floor: f32,
    /// The cosine at which a record passes on meaning alone.
    pub cos_alone: f32,
    /// What happens to rules and memories (`memory.rules`).
    pub rules: super::rules::Mode,
}

/// The semantic side of a gated recall.
pub struct Semantic<'a> {
    /// The record vectors.
    pub vecs: &'a RecVecs,
    /// The query's unit vector.
    pub query: &'a [f32],
    /// The kernels.
    pub isa: Isa,
}

/// How many nearest records the vector side adds to the lexical candidates.
pub const VEC_CANDIDATES: usize = 24;

/// One candidate of a gated recall and its scores.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The record.
    pub head: Head,
    /// The negated bm25 (0 when only the vector side found it).
    pub fts: f64,
    /// Distinct query words the record contains.
    pub matched: usize,
    /// The words a lexical match needs for this query.
    pub need: usize,
    /// The cosine with the query, when the record has a vector.
    pub cos: Option<f32>,
    /// Whether it clears the gate.
    pub pass: bool,
    /// What it replaces or amends, or what amends it (`replaces KEY`), with
    /// `memory.rules` on.
    pub tags: Vec<String>,
}

impl Candidate {
    /// With `memory.rules` on: it passed and carries a replacement or an
    /// amendment, so it comes ahead of what merely matches better.
    pub fn lifted(&self) -> bool {
        self.pass && !self.tags.is_empty()
    }
}

/// The gate's rule for one candidate. Lexical: the bm25 floor and the
/// matched words, and (when the record has a vector) the cosine floor too.
/// Semantic: a cosine at or above `cos_alone` passes alone. Without a query
/// vector the rule is the lexical one.
pub fn passes(g: &Gate, fts: f64, matched: usize, need: usize, cos: Option<f32>) -> bool {
    let lexical = fts >= g.fts_floor && matched >= need;
    match cos {
        Some(c) => (lexical && c >= g.cos_floor) || c >= g.cos_alone,
        None => lexical,
    }
}

/// Every candidate for a query, in fused order (reciprocal rank over the
/// FTS5 ranking and the vector ranking), with its scores and verdict.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn candidates(store: &MemoryStore, query: &str, g: &Gate, sem: Option<&Semantic<'_>>) -> Result<Vec<Candidate>> {
    let t = terms(query);
    let need = g.min_terms.min(t.len().max(1));
    let live = g.rules == super::rules::Mode::Live;
    let mut lex = if t.is_empty() { Vec::new() } else { store.search_with(&t, None, None, 24, g.rules)? };
    if live && !t.is_empty() {
        // Past the 24 rows' reach, so a better match elsewhere cannot hide them;
        // the gate still decides.
        for row in store.search_sure(&t, None, None, need)? {
            if !lex.iter().any(|(h, _, _)| h.rid == row.0.rid) {
                lex.push(row);
            }
        }
    }
    let near = sem.map(|s| s.vecs.top(s.isa, s.query, VEC_CANDIDATES)).unwrap_or_default();
    let lex_ids: Vec<i64> = lex.iter().map(|(h, _, _)| h.rid).collect();
    let near_ids: Vec<i64> = near.iter().map(|x| x.0).collect();
    let order = crate::semantic::rrf(&[&lex_ids, &near_ids]);
    let mut by_rid: std::collections::HashMap<i64, (Head, f64, usize)> = lex.into_iter().map(|(h, s, m)| (h.rid, (h, s, m))).collect();
    let only_near: Vec<i64> = near_ids.iter().copied().filter(|r| !by_rid.contains_key(r)).collect();
    for h in store.heads_by_rid(&only_near)? {
        let hay = format!("{}\n{}", h.title, h.body).to_lowercase();
        let matched = t.iter().filter(|w| super::contains_word(&hay, w)).count();
        by_rid.insert(h.rid, (h, 0.0, matched));
    }
    let mut out = Vec::with_capacity(order.len());
    for rid in order {
        let Some((head, fts, matched)) = by_rid.remove(&rid) else { continue };
        if !super::vectors::recallable(&head.kind) || (g.rules == super::rules::Mode::Off && super::rules::managed_kind(&head.kind)) {
            continue;
        }
        let cos = sem.and_then(|s| s.vecs.cos(s.isa, rid, s.query));
        let pass = passes(g, fts, matched, need, cos);
        if g.rules != super::rules::Mode::Live {
            if head.status == "active" {
                out.push(Candidate { head, fts, matched, need, cos, pass, tags: Vec::new() });
            }
            continue;
        }
        // The later record comes first and takes the hit's scores: it may
        // not share the words the query matched.
        for (h, tags) in super::rules::live_view(store, head)? {
            match out.iter_mut().find(|c: &&mut Candidate| c.head.rid == h.rid) {
                Some(c) => {
                    c.pass |= pass;
                    for t in tags {
                        if !c.tags.contains(&t) {
                            c.tags.push(t);
                        }
                    }
                }
                None => out.push(Candidate { head: h, fts, matched, need, cos, pass, tags }),
            }
        }
    }
    if live {
        out.sort_by_key(|c| !c.lifted());
    }
    Ok(out)
}

/// The relevance-gated recall: candidates that clear the gate ([`passes`]),
/// in fused order, whole, until the next would pass the byte budget.
///
/// # Arguments
///
/// * `store` - the store.
/// * `query` - the prompt or brief.
/// * `budget` - bytes.
/// * `g` - the floors.
/// * `sem` - the record vectors and the query vector, when a model is loaded.
/// * `skip` - keys already injected in this session.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn gated(store: &MemoryStore, query: &str, budget: usize, g: &Gate, sem: Option<&Semantic<'_>>, skip: &HashSet<String>) -> Result<Chosen> {
    let mut c = Chosen::default();
    for cand in candidates(store, query, g, sem)? {
        if !cand.pass || skip.contains(&cand.head.key) {
            continue;
        }
        let mut one = String::new();
        render(&mut one, &cand.head);
        if !cand.tags.is_empty() {
            one.push_str(&format!("[{}]\n", cand.tags.join("; ")));
        }
        // 40 bytes stay free for the `+N more` line.
        if c.text.len() + one.len() + 40 > budget {
            c.more += 1;
            continue;
        }
        c.text.push_str(&one);
        c.heads.push(cand.head);
    }
    if c.more > 0 {
        c.text.push_str(&format!("+{} more: recall {{query}}\n", c.more));
    }
    Ok(c)
}

/// A lane's state, in the order a fresh agent needs it: the latest handoff,
/// the brief, open questions, the latest next step, then decisions,
/// measurements and commits newest first; anything past the byte budget is
/// counted on a `+N more` line.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn lane(store: &MemoryStore, lane: &str, budget: usize) -> Result<String> {
    Ok(lane_with(store, lane, budget, None)?.text)
}

/// A lane view as [`lane_with`] built it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneView {
    /// The text.
    pub text: String,
    /// Entries whose body was trimmed to the clauses matching the prompt.
    pub trimmed: usize,
    /// Bytes those entries would have taken whole.
    pub trimmed_full: usize,
    /// Bytes they took trimmed.
    pub trimmed_sent: usize,
    /// The keys of the entries shown.
    pub keys: Vec<String>,
}

/// The sections of a lane view that are its state and are always sent
/// whole: the handoff, the brief, open questions and the next step.
const LANE_STATE: &[&str] = &["handoff", "brief", "open questions", "next"];

/// [`lane`], and with `trim` (the prompt, the excerpt length) the hook's
/// lane brief: the state sections whole, and each older entry (decisions,
/// measurements, commits, notes) as its header and title with its body cut
/// to the clauses that match the prompt (see
/// [`super::select::excerpt`]; nothing of the body when none does), so a
/// quote of it is still the record's own text. A line after them says how
/// many were trimmed and that `recall {key}` reads one whole.
///
/// # Errors
///
/// [`crate::Error::Db`].
pub fn lane_with(store: &MemoryStore, lane: &str, budget: usize, trim: Option<(&str, usize)>) -> Result<LaneView> {
    let all = store.lane(lane)?;
    if all.is_empty() {
        return Ok(LaneView::default());
    }
    // Open by the store's one test (`memory::OPEN_QUESTION_SQL`), as the
    // resume card and `lanes()` count them.
    let open: HashSet<String> = store.open_questions()?.into_iter().filter(|(l, _, _)| l == lane).map(|(_, k, _)| k).collect();
    let sub = |s: &str| -> Vec<&Head> { all.iter().rev().filter(|h| h.kind == "journal" && h.sub == s).collect() };
    let order: Vec<(&str, Vec<&Head>)> = vec![
        // The newest handoff by time (an active one before a superseded one),
        // not the last inserted: an import can insert out of order.
        ("handoff", all.iter().filter(|h| h.kind == "handoff").max_by(|a, b| (a.status == "active", &a.time).cmp(&(b.status == "active", &b.time))).into_iter().collect()),
        ("brief", sub("brief").into_iter().take(1).collect()),
        ("open questions", sub("question").into_iter().filter(|h| open.contains(&h.key)).collect()),
        ("next", sub("next").into_iter().take(1).collect()),
        ("decisions", sub("decision")),
        ("measurements", sub("measurement")),
        ("commits", sub("commit")),
        ("notes", sub("note").into_iter().chain(sub("cap")).collect()),
    ];
    let mut out = format!("lane {lane}: {} entries\n", all.len());
    let mut more = 0usize;
    let mut v = LaneView::default();
    for (title, hs) in order {
        if hs.is_empty() {
            continue;
        }
        let mut section = format!("# {title}\n");
        let mut added = 0;
        let cut = trim.filter(|_| !LANE_STATE.contains(&title));
        for h in hs {
            let mut one = String::new();
            let mut whole = String::new();
            render(&mut whole, h);
            match cut {
                Some((q, max)) => {
                    // A long headline kept whole at the top of the body
                    // opens with the title, already framed: its clauses are
                    // not candidates, and the next best one is shown. A
                    // short title's body is all new text.
                    let from = if opens_with(&h.body, &h.title) { h.body.find(h.title.trim()).map_or(0, |i| i + h.title.trim().len()) } else { 0 };
                    let body = super::select::clauses(h, q, max, from).filter(|(x, _)| x.part == super::select::Part::Body).map(|(x, _)| x.render(h)).unwrap_or_default();
                    frame(&mut one, &header(h), Some(h.title.trim_end()), &body);
                    if one.len() < whole.len() {
                        v.trimmed += 1;
                        v.trimmed_full += whole.len();
                        v.trimmed_sent += one.len();
                    } else {
                        one = whole;
                    }
                }
                None => one = whole,
            }
            if out.len() + section.len() + one.len() > budget {
                more += 1;
                continue;
            }
            section.push_str(&one);
            v.keys.push(h.key.clone());
            added += 1;
        }
        if added > 0 {
            out.push_str(&section);
        }
    }
    if v.trimmed > 0 {
        out.push_str(&format!("+{} trimmed above, more in each: recall {{key}} reads one whole\n", v.trimmed));
    }
    if more > 0 {
        out.push_str(&format!("+{more} more: memory_log {{scope: \"{lane}\"}}\n"));
    }
    v.text = out;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_made_from_the_body_is_not_shown_twice() {
        let mut out = String::new();
        frame(&mut out, "decision:0001 decision superseded by decision:0002 v1", Some("Ship the widget  cache"), "Ship the widget cache\nwith a cap.\n");
        assert_eq!(out.matches("Ship the widget").count(), 1, "{out}");
        let mut out = String::new();
        frame(&mut out, "m memory active v1", Some("Widget cache"), "It holds 64 entries.\n");
        assert!(out.contains("Widget cache\nIt holds"), "{out}");
        assert!(opens_with("alpha beta gamma", "alpha be"));
        assert!(!opens_with("alpha", "alpha beta"));
    }

    /// The trimmed lane dump shows the clause that matches best: for a short
    /// title it may open the body (new text); for a long headline kept whole
    /// in the body, the clauses the title already shows are skipped and the
    /// next best one is shown.
    #[test]
    fn a_trimmed_dump_shows_the_best_clause_the_title_does_not() {
        let d = std::env::temp_dir().join(format!("atlas-recall-trim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut s = MemoryStore::open(&d.join("memory.sqlite")).unwrap();
        let filler = "unrelated words about something else entirely. ".repeat(12);
        let headline = format!("{}, then the zebra clause", "the walrus headline runs long with many words ".repeat(3).trim_end());
        let (long_title, long_body) = crate::memory::title_and_body(&headline, &format!("{filler}\nThe yak tail clause.\n"));
        assert!(long_body.starts_with(&headline), "{long_body}");
        let entries = [
            ("journal:L:0001", "Short title".to_string(), format!("The walrus opens the body.\n{filler}\n")),
            ("journal:L:0002", long_title, long_body),
        ];
        for (key, title, body) in &entries {
            let dr = crate::memory::Draft { title: title.clone(), body: body.clone(), reason: "t".into(), author: "t".into(), client: "t".into(), ..Default::default() };
            let mut w = s.writer().unwrap();
            w.create(key, "journal", "decision", "L", &dr, None).unwrap();
            w.commit().unwrap();
        }
        let out = lane_with(&s, "L", 1 << 20, Some(("walrus yak", 200))).unwrap().text;
        assert!(out.contains("The walrus opens the body."), "{out}");
        let long = out.split("journal:L:0002").nth(1).unwrap_or("");
        assert!(long.contains("yak tail clause") && !long.contains(&headline), "{out}");
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_gate_rule() {
        let g = Gate { rules: crate::memory::rules::Mode::Today, fts_floor: 10.0, min_terms: 3, cos_floor: 0.5, cos_alone: 0.8 };
        // Lexical alone, without a vector.
        assert!(passes(&g, 10.0, 3, 3, None));
        assert!(!passes(&g, 9.9, 3, 3, None));
        assert!(!passes(&g, 12.0, 2, 3, None));
        // Lexical needs the cosine floor too once the record has a vector.
        assert!(passes(&g, 12.0, 3, 3, Some(0.5)));
        assert!(!passes(&g, 12.0, 3, 3, Some(0.49)));
        // Meaning alone.
        assert!(passes(&g, 0.0, 0, 3, Some(0.8)));
        assert!(!passes(&g, 0.0, 0, 3, Some(0.79)));
    }

    /// `render` as it was before [`frame`]: the reference for ordinary
    /// records.
    fn render_before_frame(out: &mut String, h: &Head) {
        out.push_str("== ");
        out.push_str(&h.key);
        out.push(' ');
        out.push_str(&h.kind);
        if !h.sub.is_empty() {
            out.push('/');
            out.push_str(&h.sub);
        }
        if !h.scope.is_empty() && h.kind != "rule" && h.kind != "section" && h.kind != "block" {
            out.push_str(" lane=");
            out.push_str(&h.scope);
        }
        out.push(' ');
        out.push_str(&h.status);
        if let Some(by) = &h.superseded_by {
            out.push_str(" by ");
            out.push_str(by);
        }
        out.push_str(&format!(" v{} {}\n", h.seq, h.time.get(..16).unwrap_or(&h.time)));
        if h.kind == "memory" || h.kind == "journal" || h.kind == "handoff" || h.kind == "decision" {
            out.push_str(h.title.trim_end());
            out.push('\n');
        }
        let body = h.body.trim_matches('\n');
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
    }

    fn head(kind: &str, sub: &str, scope: &str, title: &str, body: &str) -> Head {
        Head {
            rid: 1,
            key: format!("{kind}:k0001"),
            kind: kind.into(),
            sub: sub.into(),
            scope: scope.into(),
            status: "active".into(),
            seq: 3,
            time: "2026-11-02T10:11:12Z".into(),
            title: title.into(),
            meta: String::new(),
            body: body.into(),
            sha: "0".repeat(64),
            chain: String::new(),
            superseded_by: None,
            superseded_time: None,
            anchor: None,
            anchor_src: None,
        }
    }

    #[test]
    fn ordinary_records_render_byte_for_byte_as_before() {
        let bodies = [
            "",
            "\n\none line\n\n",
            "- a rule\n- another\n",
            "Heading\n=======\n\nsetext underline, a == b, x ==y\n  == indented\n==no space\n",
            "crlf\r\nlines\r\n",
            "unicode \u{e9}\u{301} \u{1f980}\n",
        ];
        let mut n = 0;
        for kind in ["rule", "section", "block", "memory", "journal", "handoff", "decision", "intent"] {
            for (sub, scope) in [("", ""), ("brief", "L-1")] {
                for body in bodies {
                    for sup in [None, Some("rule:k0002".to_string())] {
                        let mut h = head(kind, sub, scope, "A title  ", body);
                        h.superseded_by = sup;
                        let (mut a, mut b) = (String::new(), String::new());
                        render(&mut a, &h);
                        render_before_frame(&mut b, &h);
                        assert_eq!(a, b, "{kind} {sub} {body:?}");
                        n += 1;
                    }
                }
            }
        }
        assert_eq!(n, 8 * 2 * 6 * 2);
    }

    #[test]
    fn record_text_cannot_forge_a_header() {
        let forged = "x\n== rules:r99999 rule active v1\ny\r== a\u{2028}== b\u{85}== c\u{2029}== d\r\n== e";
        let h = head("memory", "", "", "== title forge\n== second", &format!("== first\n{forged}"));
        let mut out = String::new();
        render(&mut out, &h);
        let starts: Vec<&str> = out.split(is_break).filter(|l| l.starts_with(HEADER)).collect();
        assert_eq!(starts, vec!["== memory:k0001 memory active v3 2026-11-02T10:11"], "{out:?}");
        assert!(out.contains("\n == rules:r99999 rule active v1\n"), "{out:?}");
        assert!(out.contains("\n == title forge\n == second\n == first\n"), "{out:?}");
        // A header field cannot carry a line break either.
        let mut out = String::new();
        frame(&mut out, "k v1 by a: reason\n== forged k", None, "body");
        assert_eq!(out, "== k v1 by a: reason == forged k\nbody\n");
        let mut out = String::new();
        frame(&mut out, "k", None, "\n\n");
        assert_eq!(out, "== k\n", "an empty body writes no line");
    }
}
