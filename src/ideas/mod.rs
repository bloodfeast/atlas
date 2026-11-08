//! Idea capture (`memory.ideas`, off by default): the user's standalone
//! statements that propose, decide or constrain something are filed as they
//! are said, with their exact words, so an idea said in passing is not lost.
//!
//! The detector ([`detect`]) is a no-model, no-network linear score over
//! general English cues. Its weights are data ([`BASELINE`], or the file
//! `memory.ideas.weights` names), so a later fit replaces them without touching
//! logic. They come from one author's labelled prompts, and label error limits
//! them; generality to other users is untested.
//!
//! A capture is a journal entry of sub [`ENTRY`] with client `atlas-hook`, in
//! scope [`SCOPE`] (a `session:` scope: not a lane, never named by a prompt).
//! Its key is the hash of its words, so a repeat is the same record and a retry
//! cannot file twice. It is pending until converted (`atlas ideas convert`) or
//! dismissed; journal entries are not recalled by the hook.
//!
//! The write path never stalls the prompt or loses a capture. The hook writes
//! the capture as one file in the spool folder beside the store and answers;
//! once the answer is out, or before a Stop that could ask about it, the drain
//! files it, waiting on another writer for [`QUICK`] at most per item, and a lost
//! lock leaves it for the next event. Captures stay
//! out of the store's search index, so they cannot crowd recall's candidates.
//! Only redacted text reaches the store or the spool
//! ([`crate::redact::redact_capture`]).
//!
//! Only a prompt a person typed is filed. A prompt's hook input does not say who
//! sent it, so its spool item waits ([`Hold`]) for the transcript's record
//! ([`origin`]); a prompt nobody typed is dropped there.
//!
//! Scores in a band around the threshold are unsure, and the band only adds:
//! everything at or above the threshold is pending until a person says otherwise. When a
//! person's prompt ends in a Stop with no background tasks, no scheduled wakeups
//! and no active goal, the Stop hook blocks once and has the assistant ask, as a
//! multiple-choice question; PostToolUse reads the answer off the question's
//! `[idea ID]` marker and stores it as a label of source [`LABEL_SOURCE`].
//! Refitting from the labels is a later, explicit step, and asked rows were
//! chosen by band, so they are not a random sample. An idea is marked asked when
//! the hook blocks: a cancelled dialog spends one of the day's questions and is
//! not asked again. An unsure row nobody asks about stays in `list --unsure`.
//!
//! The same words are one record for good: a dismissed statement said again
//! stays dismissed.

pub mod cli;
mod detect;
mod origin;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::json::Value;
use crate::memory::{Draft, MemoryStore};
use crate::queue::meta_value;
use crate::{Error, Result};

/// The journal entry kind.
pub const ENTRY: &str = "idea";
/// The scope captures are filed in.
pub const SCOPE: &str = "session:ideas";
/// The first frozen weights: `eval.rs fit` on the tune set, sha eff1f59d.
pub const BASELINE: &str = include_str!("baseline-1.txt");
/// The source of a label a person gave by answering a question.
pub const LABEL_SOURCE: &str = "live";
/// The source of a label `atlas ideas dismiss` wrote: anyone may have run it,
/// an assistant clearing a backlog included, so it is not a person's truth.
pub const DISMISS_SOURCE: &str = "dismiss";
/// The options a question offers.
const YES: &str = "Yes";
const NO: &str = "No";
/// The longest a hook waits on another writer before it spools.
pub const QUICK: Duration = Duration::from_millis(50);

const KEY: &str = "journal:ideas:";
/// Spooled items drained per event, so a backlog cannot stall one.
const DRAIN_MAX: usize = 16;
/// Pending ideas named on a surface.
const SHOWN: usize = 3;
/// How far back a surface counts and names; older ideas stay pending in
/// `atlas ideas list`. A question looks no further back either.
const SURFACE_DAYS: i64 = 7;
/// How long a capture waits for its transcript record.
const HOLD_MS: u64 = 10 * 60 * 1000;
const TITLE_CHARS: usize = 40;
/// A surface line's ceiling: about 125 context tokens.
const LINE_BYTES: usize = 300;
/// The longest capture a question may ask about: it shows the whole text, and
/// the assistant writes it out, so a longer one is never asked.
const ASK_TEXT_CHARS: usize = 600;
/// A longer prompt or answer is a paste, not a statement: it costs no redaction,
/// score or store row. The longest prompt in the tune corpus is about 10 KB.
const TEXT_MAX_BYTES: usize = 64 << 10;
/// A held capture's first wait before it reads the transcript again, and the longest.
const RETRY_FIRST_MS: u64 = 1000;
const RETRY_MAX_MS: u64 = 30_000;

/// `memory.ideas`.
#[derive(Debug, Clone, PartialEq)]
pub struct IdeaSettings {
    /// The one switch for capture, surfaces and questions.
    pub enabled: bool,
    /// A weights file in place of [`BASELINE`].
    pub weights: Option<PathBuf>,
    /// How far below the threshold a score is still unsure.
    pub band_below: f32,
    /// How far above the threshold a score is still unsure.
    pub band_above: f32,
    /// Questions asked per UTC day; 0 asks none and turns the band off. The
    /// default is 0 until the ask route is proven in a live session.
    pub asks_per_day: usize,
}

impl Default for IdeaSettings {
    fn default() -> Self {
        IdeaSettings { enabled: false, weights: None, band_below: 0.25, band_above: 0.5, asks_per_day: 0 }
    }
}

/// The detector's weights and threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    weights: [f32; detect::N],
    /// The score at or above which a text is captured.
    pub threshold: f32,
    /// The weights file's sha, first 8 hex digits, stored with each score.
    pub sha: String,
}

impl Model {
    /// Reads a weights file's text: a `weights` line and a `point wide` line.
    pub fn parse(text: &str) -> Option<Model> {
        let mut weights = None;
        let mut threshold = None;
        for l in text.lines() {
            if let Some(r) = l.strip_prefix("weights ") {
                let v: Vec<f32> = r.split_whitespace().map(str::parse).collect::<std::result::Result<_, _>>().ok()?;
                weights = Some(<[f32; detect::N]>::try_from(v).ok()?);
            } else if let Some(r) = l.strip_prefix("point wide ") {
                threshold = Some(r.trim().parse().ok()?);
            }
        }
        Some(Model { weights: weights?, threshold: threshold?, sha: crate::sha256::hex(text.as_bytes())[..8].to_string() })
    }

    /// The model `settings` names.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for an unreadable file, [`Error::InvalidArgument`] for a
    /// malformed one.
    pub fn load(weights: Option<&Path>) -> Result<Model> {
        let text = match weights {
            Some(p) => std::fs::read_to_string(p)?,
            None => BASELINE.to_string(),
        };
        Model::parse(&text).ok_or_else(|| Error::InvalidArgument("idea weights: need a weights line and a point wide line".into()))
    }

    /// A text's score.
    pub fn score(&self, text: &str) -> f32 {
        detect::score_with(text, &self.weights)
    }
}

/// Where a capture stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// At or above the threshold, or answered yes: shown on the surfaces.
    Pending,
    /// In the band below the threshold, unanswered: shown only by `list --unsure`.
    Unsure,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Unsure => "unsure",
        }
    }
}

/// What a score means under `s`: `None` is no capture; the bool is "ask".
/// Everything at or above `threshold` is pending whatever the band, so the
/// captured set is the threshold's own, and the band only adds to it.
pub fn decide(score: f32, threshold: f32, s: &IdeaSettings) -> Option<(State, bool)> {
    let asking = s.asks_per_day > 0;
    if score >= threshold {
        return Some((State::Pending, asking && score < threshold + s.band_above));
    }
    (asking && score >= threshold - s.band_below).then_some((State::Unsure, true))
}

/// Whether a prompt is the user's own words: what the eval's freeze keeps, so
/// not a slash command or a command's echo.
pub fn is_typed(prompt: &str) -> bool {
    let s = prompt.trim();
    if s.is_empty() || s.starts_with("<command-") || s.starts_with("<local-command") {
        return false;
    }
    let Some(rest) = s.strip_prefix('/') else { return true };
    let word = rest.split(char::is_whitespace).next().unwrap_or("");
    word.is_empty() || !word.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
}

/// A capture's id: its normalised words' hash. The same words are one record.
fn id_of(text: &str) -> String {
    let norm = text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    crate::sha256::hex(norm.as_bytes())[..10].to_string()
}

fn key_of(id: &str) -> String {
    format!("{KEY}{id}")
}

/// One capture, redacted.
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    /// See [`id_of`].
    pub id: String,
    /// `typed`, `ask` or `manual`.
    pub source: String,
    /// The detector's score.
    pub score: f32,
    /// Where it stands.
    pub state: State,
    /// Whether it may be asked about.
    pub ask: bool,
    /// The session it came from.
    pub session: String,
    /// The words, redacted.
    pub text: String,
    /// The weights' sha the score came from; empty for a manual capture.
    pub weights: String,
    /// The idea question this text was typed into, if any.
    pub about: String,
    /// Its transcript record never turned up: not known to be a person's.
    pub unverified: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// What a held spool item waits for: its prompt's record in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    transcript: String,
    head: String,
    since_ms: u64,
    /// The transcript's length at the hold: the prompt's record comes after it.
    from: u64,
}

impl Hold {
    fn new(transcript: &str, raw: &str) -> Hold {
        Hold { transcript: transcript.to_string(), head: origin::head_hash(raw), since_ms: now_ms(), from: transcript_len(transcript).unwrap_or(0) }
    }

    fn expired(&self) -> bool {
        now_ms().saturating_sub(self.since_ms) > HOLD_MS
    }
}

impl Capture {
    /// A capture of `raw`, which is redacted here and nowhere later.
    pub fn new(raw: &str, source: &str, session: &str, score_state: impl FnOnce(&str) -> Option<(f32, State, bool)>, deny: &[String]) -> Option<Capture> {
        let text = crate::redact::redact_capture(raw.trim(), deny);
        let (score, state, ask) = score_state(&text)?;
        Some(Capture { id: id_of(&text), source: source.into(), score, state, ask, session: session.into(), text, weights: String::new(), about: String::new(), unverified: false })
    }

    fn meta(&self) -> String {
        let mut m = format!("source: {}\nscore: {:.3}\nstate: {}\nsession: {}\nfiled: {}", self.source, self.score, self.state.name(), self.session, crate::memory::now());
        if !self.weights.is_empty() {
            m.push_str(&format!("\nweights: {}", self.weights));
        }
        if !self.about.is_empty() {
            m.push_str(&format!("\nabout: {}", self.about));
        }
        if self.unverified {
            m.push_str("\norigin: unverified");
        }
        if self.ask {
            m.push_str("\nask: 1");
        }
        m
    }

    fn to_json(&self, hold: Option<&Hold>) -> String {
        let (transcript, head, since, from) = hold.map_or(("", "", 0.0, 0.0), |h| (h.transcript.as_str(), h.head.as_str(), h.since_ms as f64, h.from as f64));
        crate::json!({"id": self.id.as_str(), "source": self.source.as_str(), "score": f64::from(self.score), "state": self.state.name(), "ask": self.ask, "session": self.session.as_str(), "text": self.text.as_str(), "weights": self.weights.as_str(), "about": self.about.as_str(), "held": hold.is_some(), "transcript": transcript, "head": head, "since_ms": since, "from": from}).to_string()
    }

    fn hold_of(v: &Value) -> Option<Hold> {
        if v.get("held").and_then(Value::as_bool) != Some(true) {
            return None;
        }
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Some(Hold { transcript: s("transcript"), head: s("head"), since_ms: v.get("since_ms").and_then(Value::as_f64).unwrap_or(0.0) as u64, from: v.get("from").and_then(Value::as_f64).unwrap_or(0.0) as u64 })
    }

    /// Filed with no origin known: listed by `list --unsure`, never surfaced,
    /// never asked about.
    fn into_unverified(mut self) -> Capture {
        self.state = State::Unsure;
        self.ask = false;
        self.unverified = true;
        self
    }

    fn from_json(v: &Value) -> Option<Capture> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Capture {
            id: s("id")?,
            source: s("source")?,
            score: v.get("score").and_then(Value::as_f64)? as f32,
            state: if s("state")? == "unsure" { State::Unsure } else { State::Pending },
            ask: v.get("ask").and_then(Value::as_bool)?,
            session: s("session")?,
            text: s("text")?,
            weights: s("weights").unwrap_or_default(),
            about: s("about").unwrap_or_default(),
            unverified: false,
        })
    }
}

/// A capture's one-line title.
fn title_of(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(no words)");
    crate::memory::short_title(first)
}

/// Files a capture. False when its words are already on file.
///
/// # Errors
///
/// [`Error::Db`], which is a busy store as well.
pub fn file(store: &mut MemoryStore, c: &Capture) -> Result<bool> {
    let d = Draft { title: title_of(&c.text), meta: c.meta(), body: c.text.clone(), reason: "filed by idea capture".into(), author: "hook".into(), client: "atlas-hook".into(), provenance: None, time: None };
    let mut w = store.writer()?;
    // Recall takes the search's top rows and drops journal entries afterwards:
    // prompt-shaped text would take their places, and a commit to the index
    // makes the next prompt reread the vocabulary.
    w.unindexed();
    match w.create(&key_of(&c.id), "journal", ENTRY, SCOPE, &d, None) {
        Ok(_) => {}
        Err(Error::AlreadyExists(_)) => return Ok(false),
        Err(e) => return Err(e),
    }
    w.commit()?;
    Ok(true)
}

/// `meta` with `key` set to `value`, in place or at the end.
fn with_line(meta: &str, key: &str, value: &str) -> String {
    let line = format!("{key}: {value}");
    let mut found = false;
    let mut out: Vec<&str> = Vec::new();
    for l in meta.lines() {
        if meta_value(l, key).is_some() {
            found = true;
            out.push(&line);
        } else {
            out.push(l);
        }
    }
    if !found {
        out.push(&line);
    }
    out.join("\n")
}

fn revise_meta(w: &mut crate::memory::Writer<'_>, key: &str, h: &crate::memory::Head, meta: String, reason: &str, author: &str) -> Result<()> {
    let d = Draft { title: h.title.clone(), meta, body: h.body.clone(), reason: reason.into(), author: author.into(), client: "atlas-hook".into(), provenance: None, time: None };
    w.unindexed();
    w.revise(key, Some(&h.sha), &d)?;
    Ok(())
}

fn head_in(w: &crate::memory::Writer<'_>, key: &str) -> Result<crate::memory::Head> {
    w.head(key).map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("idea {key:?}")),
        e => e.into(),
    })
}

/// Records a label on a capture: yes keeps it pending, no closes it. `source`
/// says who gave it: only [`LABEL_SOURCE`] is a person's answer to a question.
/// The label lives in the record's meta; the latest stands.
///
/// # Errors
///
/// [`Error::NotFound`] for an unknown id; [`Error::Db`].
pub fn set_label(store: &mut MemoryStore, id: &str, yes: bool, source: &str) -> Result<()> {
    let key = key_of(id);
    let mut w = store.writer()?;
    let h = head_in(&w, &key)?;
    let closed_as_false = h.status != "active" && meta_value(&h.meta, "label") == Some("0");
    let mut meta = with_line(&h.meta, "label", if yes { "1" } else { "0" });
    meta = with_line(&meta, "label_source", source);
    meta = with_line(&meta, "label_time", &crate::memory::now());
    if yes {
        meta = with_line(&meta, "state", State::Pending.name());
    }
    revise_meta(&mut w, &key, &h, meta, "labelled", if source == LABEL_SOURCE { "user" } else { "cli" })?;
    if !yes && h.status == "active" {
        w.retire(&key, "not an idea")?;
    } else if yes && closed_as_false {
        w.reactivate(&key, "answered yes after a dismissal")?;
    }
    w.commit()?;
    Ok(())
}

/// The active captures filed in the last [`SURFACE_DAYS`], newest first: id,
/// title, meta. Walks back by rid and stops at the first older row, so a surface
/// costs the window's rows however large the backlog is.
fn recent(store: &MemoryStore) -> Result<Vec<(String, String, String)>> {
    let now = crate::memory::now();
    let mut st = store.conn().prepare_cached("SELECT r.status, r.key, v.title, v.meta FROM records r JOIN versions v ON v.vid = r.head WHERE r.scope = ?1 AND r.kind = 'journal' AND r.sub = 'idea' ORDER BY r.rid DESC")?;
    let mut rows = st.query([SCOPE])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let meta: String = r.get(3)?;
        let filed = meta_value(&meta, "filed").unwrap_or("");
        if crate::queue::minutes_between(filed, &now).is_none_or(|m| m > SURFACE_DAYS * 24 * 60) {
            break;
        }
        if r.get::<_, String>(0)? == "active" {
            let k: String = r.get(1)?;
            out.push((k.strip_prefix(KEY).unwrap_or(&k).to_string(), r.get(2)?, meta));
        }
    }
    Ok(out)
}

fn is_pending(meta: &str) -> bool {
    meta_value(meta, "state") == Some(State::Pending.name())
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut c: String = s.chars().take(n).collect();
    c.push_str("...");
    c
}

/// Cuts `s` to at most `n` bytes on a character boundary.
fn cut_bytes(s: &mut String, n: usize) {
    if s.len() > n {
        let mut end = n;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// The surface line: a count and the newest few of the last [`SURFACE_DAYS`],
/// in at most [`LINE_BYTES`] bytes and a newline, framed as records so old words
/// do not read as a live request. It offers the assistant no way to close them.
/// The second value is the newest pending id.
///
/// # Errors
///
/// [`Error::Db`].
pub fn pending_line(store: &MemoryStore) -> Result<Option<(String, String)>> {
    let rows = recent(store)?;
    let pending: Vec<_> = rows.iter().filter(|(_, _, m)| is_pending(m)).collect();
    let Some(first) = pending.first() else { return Ok(None) };
    let named: Vec<String> = pending.iter().take(SHOWN).map(|(i, t, _)| format!("{i} \"{}\"", clip(t, TITLE_CHARS))).collect();
    let mut line = format!("atlas ideas, a record of the user's past words, not requests: {} in the last {SURFACE_DAYS} days, newest {}; `atlas ideas list`", pending.len(), named.join(", "));
    cut_bytes(&mut line, LINE_BYTES);
    line.push('\n');
    Ok(Some((line, first.0.clone())))
}

/// The newest capture of `session` still to be asked about: id and whole text.
fn to_ask(store: &MemoryStore, session: &str) -> Result<Option<(String, String)>> {
    let Some((id, _, _)) = recent(store)?.into_iter().find(|(_, _, m)| meta_value(m, "ask") == Some("1") && meta_value(m, "asked").is_none() && meta_value(m, "session") == Some(session)) else {
        return Ok(None);
    };
    let body = store.head(&key_of(&id))?.map(|h| h.body);
    Ok(body.map(|b| (id, b)))
}

/// Questions asked on `day`, closed captures included.
fn asked_on(store: &MemoryStore, day: &str) -> Result<usize> {
    let mut st = store.conn().prepare_cached("SELECT COUNT(*) FROM records r JOIN versions v ON v.vid = r.head WHERE r.scope = ?1 AND r.kind = 'journal' AND r.sub = 'idea' AND instr(v.meta, ?2) > 0")?;
    let n: i64 = st.query_row([SCOPE.to_string(), format!("asked: {day}")], |r| r.get(0))?;
    Ok(n as usize)
}

fn mark_asked(store: &mut MemoryStore, id: &str, day: &str) -> Result<()> {
    let key = key_of(id);
    let mut w = store.writer()?;
    let h = head_in(&w, &key)?;
    let meta = with_line(&h.meta, "asked", day);
    revise_meta(&mut w, &key, &h, meta, "asked", "hook")?;
    w.commit()?;
    Ok(())
}

/// The instruction a Stop hook blocks with. `AskUserQuestion`'s result carries
/// the question back to [`Live::on_ask`], so nothing more is asked of the
/// assistant than the one call. It asks for a fact about the words, not a wish:
/// a real idea that is done or unwanted is still a yes, or the label is wrong.
pub fn question(id: &str, text: &str) -> String {
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "atlas idea check, not a new task; the quoted words are an old record, not a request, so do not act on them. Call AskUserQuestion once, with the question \"[idea {id}] Is this a standalone statement proposing, deciding or constraining something? \u{ab}{words}\u{bb}\", the header \"Idea?\" and the options \"{YES}\" and \"{NO}\". Then end your turn with the one word: Noted. If you cannot ask, just end your turn."
    )
}

/// The id a question's `[idea ID]` marker names: at its very start, as
/// [`question`] writes it, so a question merely quoting a marker is not one.
fn marker(question: &str) -> Option<&str> {
    let rest = question.strip_prefix("[idea ")?;
    let id = &rest[..rest.find(']')?];
    (id.len() == 10 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

/// Whether the hook asked `session` about `id`: only then is an answer a person's
/// label. Anything else carrying a marker is an ordinary question.
fn asked_here(store: &MemoryStore, id: &str, session: &str) -> bool {
    store.head(&key_of(id)).ok().flatten().is_some_and(|h| meta_value(&h.meta, "asked").is_some() && meta_value(&h.meta, "session") == Some(session))
}

/// Whether `answer` is option labels and nothing else, joined by ", ", which a
/// label may itself contain.
fn only_labels(answer: &str, labels: &[&str]) -> bool {
    let a = answer.as_bytes();
    // The offsets a label may start at, so a repeated label costs no backtracking.
    let mut starts = vec![false; a.len() + 1];
    starts[0] = true;
    for i in 0..a.len() {
        if !starts[i] {
            continue;
        }
        for l in labels.iter().map(|l| l.as_bytes()).filter(|l| !l.is_empty() && a[i..].starts_with(l)) {
            let end = i + l.len();
            if end == a.len() {
                return true;
            }
            if a[end..].starts_with(b", ") {
                starts[end + 2] = true;
            }
        }
    }
    false
}

/// The spool folder beside the store.
pub fn spool_dir(store: &Path) -> PathBuf {
    store.with_extension("ideas-spool")
}

/// Writes one spool item whole: a crash leaves the old state or the item, never
/// part of one.
fn spool(dir: &Path, name: &str, body: &str) -> std::io::Result<()> {
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, body).is_err() {
        std::fs::create_dir_all(dir)?;
        std::fs::write(&tmp, body)?;
    }
    std::fs::rename(&tmp, dir.join(format!("{name}.json")))
}

/// One held item's wait for its transcript record.
#[derive(Debug, Clone, PartialEq)]
struct Retry {
    not_before_ms: u64,
    delay_ms: u64,
    since_ms: u64,
    transcript: String,
    len: Option<u64>,
}

/// The held items whose record was not there yet. A record that never turns up
/// (a leading paste changes a prompt's head) would otherwise reread the
/// transcript on every hook event for [`HOLD_MS`].
#[derive(Debug, Default)]
pub struct Retries(HashMap<String, Retry>);

impl Retries {
    /// Whether item `name` should not be read now: before its next try, or with
    /// its transcript no longer than last time. An expired item is always read.
    /// A Stop does not wait for the next try, as it may be the turn's last chance
    /// to ask, but a read of nothing new would still find nothing.
    fn waiting(&self, name: &str, now: u64, at_stop: bool) -> bool {
        self.0.get(name).is_some_and(|r| {
            now.saturating_sub(r.since_ms) <= HOLD_MS && ((!at_stop && now < r.not_before_ms) || transcript_len(&r.transcript) == r.len)
        })
    }

    /// `len` is the transcript's length before the read that missed: a record
    /// written since then counts as growth.
    fn missed(&mut self, name: &str, h: &Hold, now: u64, len: Option<u64>) {
        let delay_ms = self.0.get(name).map_or(RETRY_FIRST_MS, |r| (r.delay_ms * 2).min(RETRY_MAX_MS));
        self.0.insert(name.to_string(), Retry { not_before_ms: now + delay_ms, delay_ms, since_ms: h.since_ms, transcript: h.transcript.clone(), len });
    }
}

fn transcript_len(transcript: &str) -> Option<u64> {
    std::fs::metadata(transcript).ok().map(|m| m.len())
}

/// Applies spooled items to the store, captures before answers, at most
/// `max`, removing each once it is in. An item the store refuses stays for the
/// next try; the first such ends the run. Applying twice is harmless, so a
/// crash before a removal and a second drainer lose nothing. A held capture is
/// filed once its record says a person typed it, dropped when it says otherwise,
/// and left in place until then; after [`HOLD_MS`] with no record it is filed as
/// unverified if the transcript exists (a person's prompt the match missed) and
/// dropped if not (a headless run may keep none). True when the folder is empty.
///
/// # Errors
///
/// [`Error::Db`] when the store refuses an item.
pub fn drain(store: &mut MemoryStore, dir: &Path, max: usize) -> Result<bool> {
    drain_with(store, dir, max, &mut Retries::default(), false)
}

/// [`drain`], with held items that are still waiting skipped at no cost, so they
/// neither read the transcript nor use up `max`. See [`Retries::waiting`] for
/// `at_stop`.
fn drain_with(store: &mut MemoryStore, dir: &Path, max: usize, retries: &mut Retries, at_stop: bool) -> Result<bool> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(true) };
    let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".json")).collect();
    // "capture-" before "answer-", the order they must apply in.
    names.sort_by(|a, b| b.cmp(a));
    let (mut more, mut left, now) = (false, max, now_ms());
    for n in names {
        if retries.waiting(&n, now, at_stop) {
            more = true;
            continue;
        }
        if left == 0 {
            more = true;
            break;
        }
        left -= 1;
        let path = dir.join(&n);
        let v = std::fs::read_to_string(&path).ok().and_then(|t| crate::json::parse(&t).ok());
        match v.as_ref().and_then(Capture::from_json) {
            Some(c) => {
                let c = match v.as_ref().and_then(Capture::hold_of) {
                    None => Some(c),
                    Some(h) => {
                        let t = Path::new(&h.transcript);
                        let len = transcript_len(&h.transcript);
                        match origin::of_prompt(t, &h.head, h.from) {
                            origin::Origin::Person => Some(c),
                            origin::Origin::Other => None,
                            origin::Origin::Unknown if !h.expired() => {
                                retries.missed(&n, &h, now, len);
                                more = true;
                                continue;
                            }
                            origin::Origin::Unknown => t.exists().then(|| {
                                // Never asked or surfaced from here. Many of these
                                // would mean a record is written before its hook.
                                eprintln!("atlas ideas: no record of a held prompt after its hold, filed as unverified");
                                c.into_unverified()
                            }),
                        }
                    }
                };
                if let Some(c) = c {
                    file(store, &c)?;
                }
            }
            None => {
                let answer = v.as_ref().and_then(|v| Some((v.get("id")?.as_str()?.to_string(), v.get("yes")?.as_bool()?)));
                match answer {
                    Some((id, yes)) => match set_label(store, &id, yes, LABEL_SOURCE) {
                        Ok(()) | Err(Error::NotFound(_)) => {}
                        Err(e) => return Err(e),
                    },
                    // Never its contents: the file may hold private words.
                    None => eprintln!("atlas ideas: an unreadable spool item was dropped"),
                }
            }
        }
        retries.0.remove(&n);
        let _ = std::fs::remove_file(&path);
    }
    Ok(!more)
}

/// Runs `f` with the store's wait on other writers cut to [`QUICK`].
fn quick<T>(store: &mut MemoryStore, f: impl FnOnce(&mut MemoryStore) -> Result<T>) -> Result<T> {
    let _ = store.conn().busy_timeout(QUICK);
    let r = f(store);
    let _ = store.conn().busy_timeout(crate::memory::BUSY_TIMEOUT);
    r
}

/// Idea capture in a running hook state. Every method fails open: an error
/// costs a line on stderr, never the prompt, and no private text is logged.
pub struct Live {
    settings: IdeaSettings,
    model: Model,
    spool: PathBuf,
    /// The spool may hold items: true at open and after a spool.
    drain: bool,
    /// The newest pending id each session has been shown.
    shown: HashMap<String, String>,
    retries: Retries,
    /// The prompt each session was last asked at: one question a turn, whatever
    /// `stop_hook_active` says after the question's own tool call.
    asked_at: HashMap<String, String>,
}

impl Live {
    /// `None` when off, or (said on stderr) when the weights cannot be read.
    pub fn open(s: &IdeaSettings, store: &Path) -> Option<Live> {
        if !s.enabled {
            return None;
        }
        // A term must not be filed because its list could not be read.
        if crate::redact::denylist_unreadable() {
            eprintln!("atlas ideas: off, ATLAS_LEAK_DENYLIST cannot be read");
            return None;
        }
        if crate::redact::denylist().is_empty() {
            eprintln!("atlas ideas: no leak denylist, only the secret rules redact");
        }
        let model = match Model::load(s.weights.as_deref()) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("atlas ideas: off, weights: {e}");
                return None;
            }
        };
        Some(Live { settings: s.clone(), model, spool: spool_dir(store), drain: true, shown: HashMap::new(), retries: Retries::default(), asked_at: HashMap::new() })
    }

    /// Whether questions may be asked at all.
    pub fn asks(&self) -> bool {
        self.settings.asks_per_day > 0
    }

    fn drain_quick(&mut self, store: &mut MemoryStore, at_stop: bool) {
        if !self.drain {
            return;
        }
        let (dir, retries) = (self.spool.clone(), &mut self.retries);
        match quick(store, |s| drain_with(s, &dir, DRAIN_MAX, retries, at_stop)) {
            Ok(done) => self.drain = !done,
            Err(e) => eprintln!("atlas ideas: spool not drained: {e}"),
        }
    }

    fn capture(&mut self, store: &mut MemoryStore, raw: &str, source: &str, session: &str, may_ask: bool, hold: Option<Hold>) {
        let (model, settings) = (&self.model, &self.settings);
        let judged = |text: &str| {
            let score = model.score(text);
            let (state, ask) = decide(score, model.threshold, settings)?;
            match state {
                State::Unsure if !may_ask => None,
                _ => Some((score, state, ask && may_ask && text.chars().count() <= ASK_TEXT_CHARS)),
            }
        };
        let Some(mut c) = Capture::new(raw, source, session, judged, crate::redact::denylist()) else { return };
        c.weights = self.model.sha.clone();
        let direct = if hold.is_some() { c.clone().into_unverified() } else { c.clone() };
        self.enqueue(store, &format!("capture-{}", c.id), &c.to_json(hold.as_ref()), |s| file(s, &direct).map(|_| ()));
    }

    /// Spools one item for [`Live::after`]; if the spool itself fails, tries the
    /// store. Costs a file write here, and a commit only once the answer is out.
    fn enqueue(&mut self, store: &mut MemoryStore, name: &str, body: &str, apply: impl FnOnce(&mut MemoryStore) -> Result<()>) {
        match spool(&self.spool, name, body) {
            Ok(()) => self.drain = true,
            Err(e) => {
                eprintln!("atlas ideas: spool: {e}");
                if let Err(e) = quick(store, apply) {
                    eprintln!("atlas ideas: an item was lost: {e}");
                }
            }
        }
    }

    /// Work that waits until the hook's answer is sent: the spool goes to the
    /// store. A lost lock leaves it for the next event.
    pub fn after(&mut self, store: &mut MemoryStore) {
        self.drain_quick(store, false);
    }

    /// A prompt (UserPromptSubmit, not a task notification). Held until the
    /// transcript says a person typed it.
    pub fn on_prompt(&mut self, store: &mut MemoryStore, session: &str, prompt: &str, transcript: &str) {
        if prompt.len() <= TEXT_MAX_BYTES && is_typed(prompt) {
            self.capture(store, prompt, "typed", session, true, Some(Hold::new(transcript, prompt)));
        }
    }

    /// A finished `AskUserQuestion`: an answer to an idea question becomes a
    /// label; a free-text answer to any other question is a capture of its own.
    pub fn on_ask(&mut self, store: &mut MemoryStore, session: &str, input: &Value, response: &Value) {
        let answers = response.get("answers").or_else(|| input.get("answers")).and_then(Value::as_object);
        let Some(answers) = answers else { return };
        let questions = response.get("questions").or_else(|| input.get("questions")).and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
        for (q, v) in answers.iter() {
            // A multi-select answer arrives as a list.
            let joined = match v.as_array() {
                Some(l) => l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
                None => v.as_str().unwrap_or("").to_string(),
            };
            let a = joined.trim();
            if a.is_empty() || a.len() > TEXT_MAX_BYTES {
                continue;
            }
            if let Some(id) = marker(q).filter(|id| asked_here(store, id, session)) {
                let id = id.to_string();
                let yes = if a.eq_ignore_ascii_case(YES) {
                    true
                } else if a.eq_ignore_ascii_case(NO) {
                    false
                } else {
                    // Free text is no label, but a person's own words: filed
                    // whatever the detector says, naming the question.
                    self.typed_answer(store, a, session, &id);
                    continue;
                };
                let body = crate::json!({"id": id.as_str(), "yes": yes}).to_string();
                self.enqueue(store, &format!("answer-{id}"), &body, |s| set_label(s, &id, yes, LABEL_SOURCE));
                continue;
            }
            let labels: Vec<&str> = questions
                .iter()
                .find(|x| x.get("question").and_then(Value::as_str) == Some(q.as_str()))
                .and_then(|x| x.get("options")?.as_array())
                .map(|o| o.iter().filter_map(|x| x.get("label")?.as_str()).collect())
                .unwrap_or_default();
            if only_labels(a, &labels) {
                continue;
            }
            self.capture(store, a, "ask", session, false, None);
        }
    }

    fn typed_answer(&mut self, store: &mut MemoryStore, raw: &str, session: &str, about: &str) {
        let model = &self.model;
        let Some(mut c) = Capture::new(raw, "ask", session, |t: &str| Some((model.score(t), State::Pending, false)), crate::redact::denylist()) else { return };
        c.weights = self.model.sha.clone();
        c.about = about.to_string();
        let direct = c.clone();
        self.enqueue(store, &format!("capture-{}", c.id), &c.to_json(None), |s| file(s, &direct).map(|_| ()));
    }

    /// A Stop: the instruction to ask one question, or `None`. A dialog holds
    /// the main thread, so it asks only with nothing else waiting on it: `idle`
    /// (no background tasks, no scheduled wakeups), no active goal, and a turn a
    /// person started and has not been asked in yet, under the daily cap. The
    /// transcript is read only when there may be something to ask, and the goal
    /// search has no floor: a goal missed would hold its session with a dialog.
    /// A turn that could be asked drains the spool before the answer goes out,
    /// since a turn with no tool call has had no event since the prompt and its
    /// capture must be filed to be asked; every other Stop leaves that to
    /// [`Live::after`].
    pub fn on_stop(&mut self, store: &mut MemoryStore, session: &str, stop_hook_active: bool, idle: bool, transcript: &str) -> Option<String> {
        if stop_hook_active || !idle || self.settings.asks_per_day == 0 {
            return None;
        }
        let day = crate::memory::now().chars().take(10).collect::<String>();
        let (cap, spooled) = (self.settings.asks_per_day, self.drain);
        let may_ask = quick(store, |s| Ok(asked_on(s, &day)? < cap && (spooled || to_ask(s, session)?.is_some())));
        if !may_ask.unwrap_or_else(|e| {
            eprintln!("atlas ideas: no question this turn: {e}");
            false
        }) {
            return None;
        }
        let t = Path::new(transcript);
        let (true, turn) = origin::last_prompt(t)? else { return None };
        if self.asked_at.get(session) == Some(&turn) {
            return None;
        }
        self.drain_quick(store, true);
        let asked_at = &mut self.asked_at;
        let r = quick(store, |s| {
            let Some((id, text)) = to_ask(s, session)? else { return Ok(None) };
            if crate::resume::goal_anywhere(t).is_some() {
                return Ok(None);
            }
            // Again: the drain and the goal search are a wide window for another
            // session's question.
            if asked_on(s, &day)? >= cap {
                return Ok(None);
            }
            mark_asked(s, &id, &day)?;
            asked_at.insert(session.to_string(), turn);
            Ok(Some(question(&id, &text)))
        });
        r.unwrap_or_else(|e| {
            eprintln!("atlas ideas: no question this turn: {e}");
            None
        })
    }

    /// The line for a check-in or task notification: only when this session
    /// has not been shown the newest pending idea, so a quiet store costs
    /// nothing and the most a day can cost is one line per capture.
    pub fn line(&mut self, store: &mut MemoryStore, session: &str) -> Option<String> {
        match pending_line(store) {
            Ok(Some((line, newest))) => {
                if self.shown.get(session) == Some(&newest) {
                    return None;
                }
                self.shown.insert(session.to_string(), newest);
                Some(line)
            }
            Ok(None) => None,
            Err(e) => {
                eprintln!("atlas ideas: {e}");
                None
            }
        }
    }

    /// The resume card's line: the last [`SURFACE_DAYS`]' count and newest ids.
    pub fn resume_line(&self, store: &MemoryStore) -> Option<String> {
        let rows = recent(store).ok()?;
        let pending: Vec<&str> = rows.iter().filter(|(_, _, m)| is_pending(m)).map(|(i, _, _)| i.as_str()).collect();
        let n = pending.len();
        (n > 0).then(|| format!("- ideas on record from the last {SURFACE_DAYS} days, not requests: {n}, newest {}; `atlas ideas list`", pending[..n.min(SHOWN)].join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(d: &Path) -> MemoryStore {
        MemoryStore::open(&d.join("memory.sqlite")).unwrap()
    }

    fn on() -> IdeaSettings {
        IdeaSettings { enabled: true, asks_per_day: 5, ..IdeaSettings::default() }
    }

    fn live(d: &Path, s: &IdeaSettings) -> Live {
        Live::open(s, &d.join("memory.sqlite")).unwrap()
    }

    /// Hands the prompt to `l`, then writes a person's record of `text`: the
    /// hook runs before the record is written.
    fn typed(l: &mut Live, s: &mut MemoryStore, d: &Path, text: &str) -> String {
        typed_in(l, s, d, "s", text)
    }

    fn typed_in(l: &mut Live, s: &mut MemoryStore, d: &Path, session: &str, text: &str) -> String {
        let t = d.join("transcript.jsonl");
        let path = t.to_string_lossy().into_owned();
        l.on_prompt(s, session, text, &path);
        let line = crate::json!({"type": "user", "message": {"content": text}, "origin": {"kind": "human"}, "entrypoint": "cli"}).to_string();
        let mut old = std::fs::read_to_string(&t).unwrap_or_default();
        old.push_str(&line);
        old.push('\n');
        std::fs::write(&t, old).unwrap();
        path
    }

    /// Every active capture of the last [`SURFACE_DAYS`].
    fn active(s: &MemoryStore) -> Result<Vec<(String, String, String)>> {
        recent(s)
    }

    const IDEA: &str = "we should keep the cache small because a large one would cost more than it saves";

    #[test]
    fn the_baseline_parses_with_its_frozen_threshold_and_a_broken_file_is_refused() {
        let m = Model::load(None).unwrap();
        assert_eq!(m.threshold, -1.658);
        assert!(Model::parse("weights 1 2\npoint wide 1").is_none());
        assert!(Model::parse("point wide 1").is_none());
    }

    #[test]
    fn off_opens_nothing() {
        assert!(Live::open(&IdeaSettings::default(), Path::new("x")).is_none());
        assert_eq!(IdeaSettings::default().asks_per_day, 0, "nothing asks until the route is proven live");
        let s = IdeaSettings { weights: Some("no such file".into()), ..on() };
        assert!(Live::open(&s, Path::new("x")).is_none());
    }

    #[test]
    fn the_band_only_adds_to_the_wide_set() {
        let s = IdeaSettings { band_below: 0.5, band_above: 0.25, asks_per_day: 3, ..on() };
        assert_eq!(decide(1.0, 0.0, &s), Some((State::Pending, false)));
        assert_eq!(decide(0.1, 0.0, &s), Some((State::Pending, true)));
        assert_eq!(decide(-0.1, 0.0, &s), Some((State::Unsure, true)));
        assert_eq!(decide(-0.6, 0.0, &s), None);
        // No asking, no band: the threshold alone.
        let s = IdeaSettings { asks_per_day: 0, ..s };
        assert_eq!(decide(0.1, 0.0, &s), Some((State::Pending, false)));
        assert_eq!(decide(-0.1, 0.0, &s), None);
    }

    #[test]
    fn typed_means_what_the_corpus_means() {
        for t in ["we should do this", "/Users/x is the folder"] {
            assert!(is_typed(t), "{t}");
        }
        for t in ["", "  ", "/compact", "/goal be done", "<command-name>x</command-name>", "<local-command-stdout>y"] {
            assert!(!is_typed(t), "{t}");
        }
    }

    #[test]
    fn a_marker_is_read_exactly() {
        assert_eq!(marker("[idea 0123456789] Keep?"), Some("0123456789"));
        for q in ["Keep?", "Keep? [idea abcdef0123]", "[idea short]", "[idea 0123456789", "[idea zzzzzzzzzz]"] {
            assert_eq!(marker(q), None, "{q}");
        }
    }

    #[test]
    fn a_repeat_is_one_record_and_labels_close_and_reopen() {
        let d = tempfile_dir("repeat");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        typed_in(&mut l, &mut s, &d, "s1", IDEA);
        l.after(&mut s);
        typed_in(&mut l, &mut s, &d, "s2", &IDEA.to_uppercase());
        l.after(&mut s);
        let rows = active(&s).unwrap();
        assert_eq!(rows.len(), 1);
        let id = rows[0].0.clone();
        let h = s.head(&key_of(&id)).unwrap().unwrap();
        assert_eq!((h.sub.as_str(), h.scope.as_str(), h.body.as_str()), (ENTRY, SCOPE, IDEA));
        set_label(&mut s, &id, false, DISMISS_SOURCE).unwrap();
        assert!(active(&s).unwrap().is_empty());
        let h = s.head(&key_of(&id)).unwrap().unwrap();
        assert_eq!((meta_value(&h.meta, "label"), meta_value(&h.meta, "label_source")), (Some("0"), Some(DISMISS_SOURCE)));
        // A dismissed statement said again stays dismissed.
        typed(&mut l, &mut s, &d, IDEA);
        l.after(&mut s);
        assert!(active(&s).unwrap().is_empty());
        set_label(&mut s, &id, true, LABEL_SOURCE).unwrap();
        assert_eq!(active(&s).unwrap().len(), 1);
    }

    #[test]
    fn a_term_leaves_nothing_in_record_or_spool() {
        let deny = vec!["zebrafish".to_string()];
        let judged = |_: &str| Some((1.0, State::Pending, false));
        let c = Capture::new("we should move the ZebraFish plan\nand keep this", "typed", "s", judged, &deny).unwrap();
        assert_eq!(c.text, "we should move the [redacted] plan\nand keep this");
        let d = tempfile_dir("term");
        let dir = spool_dir(&d.join("memory.sqlite"));
        spool(&dir, "capture-x", &c.to_json(Some(&Hold::new("t", "we should move the ZebraFish plan")))).unwrap();
        let held = std::fs::read_to_string(dir.join("capture-x.json")).unwrap();
        assert!(!held.to_lowercase().contains("zebrafish"), "{held}");
        assert!(!c.meta().to_lowercase().contains("zebrafish"));
    }

    #[test]
    fn a_lost_lock_leaves_the_spool_and_a_later_event_drains() {
        let d = tempfile_dir("lock");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        let spooled = || std::fs::read_dir(spool_dir(&d.join("memory.sqlite"))).unwrap().count();
        let mut other = store(&d);
        let held = other.writer().unwrap();
        let t = std::time::Instant::now();
        typed(&mut l, &mut s, &d, IDEA);
        l.after(&mut s);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "well under the store's 30 s wait: {:?}", t.elapsed());
        assert_eq!(spooled(), 1);
        assert!(active(&s).unwrap().is_empty());
        drop(held);
        drop(other);
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1);
        assert_eq!(spooled(), 0);
    }

    #[test]
    fn a_crash_before_the_drain_loses_nothing() {
        let d = tempfile_dir("nodrain");
        let mut s = store(&d);
        typed(&mut live(&d, &on()), &mut s, &d, IDEA);
        assert!(active(&s).unwrap().is_empty());
        // A new process: its first event's drain files what the last one spooled.
        let mut l = live(&d, &on());
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1);
    }

    #[test]
    fn a_capture_stays_out_of_the_search_index() {
        let d = tempfile_dir("fts");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        typed(&mut l, &mut s, &d, IDEA);
        l.after(&mut s);
        let id = active(&s).unwrap()[0].0.clone();
        set_label(&mut s, &id, true, LABEL_SOURCE).unwrap();
        let n: i64 = s.conn().query_row("SELECT COUNT(*) FROM records_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn a_crash_between_spool_and_drain_loses_nothing_and_a_crash_after_filing_files_once() {
        let d = tempfile_dir("crash");
        let mut s = store(&d);
        let judged = |_: &str| Some((1.0, State::Pending, false));
        let c = Capture::new(IDEA, "typed", "s", judged, &[]).unwrap();
        let dir = spool_dir(&d.join("memory.sqlite"));
        // The process died here: a spooled capture, and the same one already filed
        // under other words' item, as a crash before the removal leaves it.
        spool(&dir, &format!("capture-{}", c.id), &c.to_json(None)).unwrap();
        file(&mut s, &c).unwrap();
        let other = Capture { id: "aaaaaaaaaa".into(), text: "another plan worth keeping for later".into(), ..c.clone() };
        spool(&dir, "capture-aaaaaaaaaa", &other.to_json(None)).unwrap();
        spool(&dir, "answer-aaaaaaaaaa", &crate::json!({"id": "aaaaaaaaaa", "yes": false}).to_string()).unwrap();
        // A new process opens the store: the first event drains.
        let mut l = live(&d, &on());
        typed_in(&mut l, &mut s, &d, "s2", "A");
        l.after(&mut s);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let rows = active(&s).unwrap();
        assert_eq!(rows.len(), 1, "the answered one is closed, the filed one is one record");
        assert!(s.head(&key_of("aaaaaaaaaa")).unwrap().is_some());
    }

    #[test]
    fn a_marker_on_a_question_the_hook_never_asked_is_no_label() {
        let d = tempfile_dir("spoof");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        typed(&mut l, &mut s, &d, IDEA);
        l.after(&mut s);
        let id = active(&s).unwrap()[0].0.clone();
        let answer = |q: String| crate::json::parse(&format!("{{\"answers\":{{\"{q}\":\"No\"}}}}")).unwrap();
        // At the start of a question, but never asked; then quoted, not at the start.
        l.on_ask(&mut s, "s", &answer(format!("[idea {id}] Is it?")), &Value::Null);
        l.on_ask(&mut s, "s", &answer(format!("Is [idea {id}] it?")), &Value::Null);
        l.after(&mut s);
        let rows = active(&s).unwrap();
        assert_eq!(meta_value(&rows.iter().find(|r| r.0 == id).unwrap().2, "label"), None);
        // Asked in this session: another session's answer is still no label.
        mark_asked(&mut s, &id, "2026-01-01").unwrap();
        l.on_ask(&mut s, "t", &answer(format!("[idea {id}] Is it?")), &Value::Null);
        l.after(&mut s);
        assert_eq!(meta_value(&active(&s).unwrap().iter().find(|r| r.0 == id).unwrap().2, "label"), None);
        l.on_ask(&mut s, "s", &answer(format!("[idea {id}] Is it?")), &Value::Null);
        l.after(&mut s);
        assert!(active(&s).unwrap().iter().all(|r| r.0 != id), "the person's no closes it");
    }

    #[test]
    fn a_turn_with_no_tool_call_is_still_asked_about() {
        let d = tempfile_dir("no-tool");
        let mut s = store(&d);
        let wide = IdeaSettings { band_below: 100.0, band_above: 100.0, asks_per_day: 9, ..on() };
        let mut l = live(&d, &wide);
        // No `after` between the prompt and Stop: the capture is still in the spool.
        let t = typed(&mut l, &mut s, &d, "first plan we could try next");
        assert!(active(&s).unwrap().is_empty());
        assert!(l.on_stop(&mut s, "s", false, true, &t).is_some());
    }

    #[test]
    fn one_question_a_turn_and_a_goal_far_back_still_counts() {
        let d = tempfile_dir("turn");
        let mut s = store(&d);
        let wide = IdeaSettings { band_below: 100.0, band_above: 100.0, asks_per_day: 9, ..on() };
        let mut l = live(&d, &wide);
        typed(&mut l, &mut s, &d, "first plan we could try next");
        let t = typed(&mut l, &mut s, &d, "second plan we could try next");
        l.after(&mut s);
        assert!(l.on_stop(&mut s, "s", false, true, &t).is_some());
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None, "the same turn, after the question's tool call");

        // An unmet goal more than the resume card's search back holds a question.
        let d = tempfile_dir("far-goal");
        let mut s = store(&d);
        let mut l = live(&d, &wide);
        let path = d.join("transcript.jsonl");
        let mut old = "{\"type\":\"attachment\",\"attachment\":{\"type\":\"goal_status\",\"met\":false,\"sentinel\":true,\"condition\":\"an invented goal\"}}\n".to_string();
        old.push_str(&format!("{{\"type\":\"assistant\",\"pad\":\"{}\"}}\n", "x".repeat(5 << 20)));
        std::fs::write(&path, old).unwrap();
        let t = typed(&mut l, &mut s, &d, "third plan we could try next");
        l.after(&mut s);
        assert_eq!(crate::resume::goal(Path::new(&t)), None, "past the card's search");
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None);
    }

    #[test]
    fn a_held_item_waits_without_reading_and_does_not_block_the_rest() {
        let d = tempfile_dir("retry");
        let t = d.join("t.jsonl");
        std::fs::write(&t, "{}\n").unwrap();
        let path = t.to_string_lossy().into_owned();
        let now = now_ms();
        let h = Hold { transcript: path.clone(), head: String::new(), since_ms: now, from: 0 };
        let mut r = Retries::default();
        assert!(!r.waiting("a", now, false), "never tried");
        let read_at = transcript_len(&path);
        r.missed("a", &h, now, read_at);
        assert!(r.waiting("a", now + RETRY_FIRST_MS - 1, false), "before its next try");
        assert!(r.waiting("a", now + RETRY_FIRST_MS, false), "the transcript has not grown");
        assert!(r.waiting("a", now + 1, true), "a Stop too reads nothing new for nothing");
        std::fs::write(&t, "{}\n{}\n").unwrap();
        assert!(r.waiting("a", now + RETRY_FIRST_MS - 1, false), "grown, but before its next try");
        assert!(!r.waiting("a", now + 1, true), "grown: a Stop does not wait for its next try");
        assert!(!r.waiting("a", now + RETRY_FIRST_MS, false), "grown and due");
        assert!(!r.waiting("a", now + HOLD_MS + 1, false), "expired: read to settle it");
        // A record written after the read that missed, and before the miss was
        // noted, is growth: the length is the one from before the read.
        r.missed("a", &h, now, read_at);
        assert!(!r.waiting("a", now + RETRY_FIRST_MS * 2, false), "written between the read and the miss");
        for _ in 0..9 {
            r.missed("a", &h, now, read_at);
        }
        assert_eq!(r.0["a"].delay_ms, RETRY_MAX_MS);

        // More held items than one drain takes never keep a later capture out.
        let mut s = store(&d);
        let mut l = live(&d, &on());
        for i in 0..(DRAIN_MAX + 2) {
            l.on_prompt(&mut s, "s", &format!("we should try invented plan number {i} because it costs little"), &path);
        }
        l.after(&mut s);
        assert_eq!(std::fs::read_dir(spool_dir(&d.join("memory.sqlite"))).unwrap().count(), DRAIN_MAX + 2, "all still held");
        let late = "we should write the invented late plan down because it costs little";
        let line = crate::json!({"type": "user", "message": {"content": late}, "origin": {"kind": "human"}, "entrypoint": "cli"}).to_string();
        l.on_prompt(&mut s, "s", late, &path);
        std::fs::write(&t, format!("{{}}\n{{}}\n{line}\n")).unwrap();
        for _ in 0..3 {
            l.after(&mut s);
        }
        assert_eq!(active(&s).unwrap().len(), 1);
    }

    #[test]
    fn a_paste_past_the_limit_is_not_captured() {
        let d = tempfile_dir("paste");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        let big = format!("{IDEA} {}", "word ".repeat(TEXT_MAX_BYTES / 5 + 1));
        typed(&mut l, &mut s, &d, &big);
        l.after(&mut s);
        assert!(active(&s).unwrap().is_empty());
        assert_eq!(std::fs::read_dir(spool_dir(&d.join("memory.sqlite"))).map_or(0, Iterator::count), 0);
    }

    #[test]
    fn free_text_in_a_multi_select_list_is_filed() {
        let d = tempfile_dir("list");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        let input = crate::json!({"questions": [{"question": "Which?", "options": [{"label": "A"}, {"label": "B"}]}], "answers": {"Which?": ["A", "we should pick neither because both cost too much to keep"]}});
        l.on_ask(&mut s, "s", &input, &Value::Null);
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1);
        let picked = crate::json!({"questions": [{"question": "Which?", "options": [{"label": "A"}, {"label": "B"}]}], "answers": {"Which?": ["A", "B"]}});
        l.on_ask(&mut s, "s", &picked, &Value::Null);
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1, "options picked are no words of their own");
    }

    #[test]
    fn a_question_asks_about_the_whole_text_within_the_cap() {
        let d = tempfile_dir("ask");
        let mut s = store(&d);
        let wide = IdeaSettings { band_below: 100.0, band_above: 100.0, asks_per_day: 2, ..on() };
        let mut l = live(&d, &wide);
        // Nothing filed: nothing asked.
        assert_eq!(l.on_stop(&mut s, "s", false, true, ""), None);
        let t = typed(&mut l, &mut s, &d, "first plan we could try next");
        l.after(&mut s);
        // The hook's own block coming round again asks nothing.
        assert_eq!(l.on_stop(&mut s, "s", true, true, &t), None);
        // Something running asks nothing, and the capture is still there to ask.
        assert_eq!(l.on_stop(&mut s, "s", false, false, &t), None);
        let q = l.on_stop(&mut s, "s", false, true, &t).unwrap();
        assert!(q.contains("AskUserQuestion") && q.contains("[idea ") && q.contains("first plan we could try next"), "{q}");
        assert!(q.contains("standalone statement proposing, deciding or constraining"), "{q}");
        assert!(!q.contains("Keep this"), "{q}");
        // Asked once.
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None);
        typed(&mut l, &mut s, &d, "second plan we could try next");
        l.after(&mut s);
        assert!(l.on_stop(&mut s, "s", false, true, &t).is_some());
        // The cap holds across prompts and processes.
        typed(&mut l, &mut s, &d, "third plan we could try next");
        l.after(&mut s);
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None);
        let mut again = live(&d, &wide);
        typed(&mut again, &mut s, &d, "fourth plan we could try next");
        again.after(&mut s);
        assert_eq!(again.on_stop(&mut s, "s", false, true, &t), None);
        // Another session's capture is not this one's to ask about.
        let wide = IdeaSettings { asks_per_day: 9, ..wide };
        let mut fresh = live(&d, &wide);
        assert_eq!(fresh.on_stop(&mut s, "t", false, true, &t), None);
    }

    #[test]
    fn no_question_after_a_turn_no_person_started_or_with_a_goal_active() {
        let d = tempfile_dir("nobody");
        let mut s = store(&d);
        let wide = IdeaSettings { band_below: 100.0, band_above: 100.0, asks_per_day: 9, ..on() };
        let mut l = live(&d, &wide);
        let t = typed(&mut l, &mut s, &d, "one more plan we could try");
        l.after(&mut s);
        let mut text = std::fs::read_to_string(&t).unwrap();
        let clean = text.clone();
        // A loop's iteration started this turn.
        text.push_str("{\"type\":\"user\",\"isMeta\":true,\"message\":{\"content\":\"tick\"}}\n");
        std::fs::write(&t, &text).unwrap();
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None);
        // A goal is active: its own Stop hook is running this session.
        let goal = "{\"type\":\"attachment\",\"attachment\":{\"type\":\"goal_status\",\"met\":false,\"sentinel\":true,\"condition\":\"an invented goal\"}}\n";
        std::fs::write(&t, format!("{goal}{clean}")).unwrap();
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None);
        // No transcript: nothing known, nothing asked.
        assert_eq!(l.on_stop(&mut s, "s", false, true, "Z:/no/such/file.jsonl"), None);
        // The same state with neither: it asks, and nothing was spent before.
        std::fs::write(&t, clean).unwrap();
        assert!(l.on_stop(&mut s, "s", false, true, &t).is_some());
    }

    /// The band wide open and asks on, with a transcript at `d`.
    fn asking(d: &Path) -> (MemoryStore, Live, String) {
        let s = store(d);
        let l = live(d, &IdeaSettings { band_below: 100.0, band_above: 100.0, asks_per_day: 9, ..on() });
        (s, l, d.join("transcript.jsonl").to_string_lossy().into_owned())
    }

    fn append(path: &str, line: &str) {
        let mut text = std::fs::read_to_string(path).unwrap_or_default();
        text.push_str(line);
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_goal_that_finished_no_longer_holds_the_question() {
        let d = tempfile_dir("goal-done");
        let (mut s, mut l, t) = asking(&d);
        append(&t, r#"{"type":"attachment","attachment":{"type":"goal_status","met":false,"sentinel":true,"condition":"an invented goal"}}"#);
        typed(&mut l, &mut s, &d, "a plan while the goal runs");
        l.after(&mut s);
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None, "the goal is running");
        // Its completion record, as Claude Code writes it, with no later clear.
        append(&t, r#"{"type":"attachment","attachment":{"type":"goal_status","met":true,"iterations":4,"durationMs":9000,"tokens":1200}}"#);
        assert!(l.on_stop(&mut s, "s", false, true, &t).is_some(), "finished: this session asks again");
    }

    #[test]
    fn a_prompt_sharing_a_head_with_an_older_one_answers_to_its_own_record() {
        let shared = "we could try the plan ".repeat(12);
        let (first, second) = (format!("{shared} and this tail"), format!("{shared} and that tail"));
        assert_eq!(origin::head_hash(&first), origin::head_hash(&second));
        let peer = |text: &str| crate::json!({"type": "user", "isMeta": true, "message": {"content": text}, "origin": {"kind": "peer"}, "entrypoint": "cli"}).to_string();
        let person = |text: &str| crate::json!({"type": "user", "message": {"content": text}, "origin": {"kind": "human"}, "entrypoint": "cli"}).to_string();

        // A peer's prompt after a typed one: its record lags the first drain.
        let d = tempfile_dir("head-peer");
        let (mut s, mut l, t) = asking(&d);
        typed(&mut l, &mut s, &d, &first);
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1);
        l.on_prompt(&mut s, "s", &second, &t);
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1, "not taken for the older typed prompt");
        append(&t, &peer(&second));
        l.retries = Retries::default();
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1, "dropped on its own record");
        assert_eq!(std::fs::read_dir(spool_dir(&d.join("memory.sqlite"))).unwrap().count(), 0);

        // The mirror: a person's prompt after a peer's is kept, and can be asked.
        let d = tempfile_dir("head-person");
        let (mut s, mut l, t) = asking(&d);
        append(&t, &peer(&first));
        l.on_prompt(&mut s, "s", &second, &t);
        l.after(&mut s);
        assert_eq!(std::fs::read_dir(spool_dir(&d.join("memory.sqlite"))).unwrap().count(), 1, "not dropped on the older record");
        append(&t, &person(&second));
        assert!(l.on_stop(&mut s, "s", false, true, &t).is_some(), "the Stop reads the grown transcript at once");
        assert_eq!(active(&s).unwrap().len(), 1);
    }

    #[test]
    fn a_stop_that_cannot_ask_leaves_the_spool_for_after() {
        let spooled = |d: &Path| std::fs::read_dir(spool_dir(&d.join("memory.sqlite"))).unwrap().count();
        // A turn no person started.
        let d = tempfile_dir("gate-person");
        let (mut s, mut l, t) = asking(&d);
        typed(&mut l, &mut s, &d, "a plan held in the spool");
        append(&t, r#"{"type":"user","isMeta":true,"message":{"content":"tick"}}"#);
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None);
        assert_eq!(spooled(&d), 1);
        l.after(&mut s);
        assert_eq!(spooled(&d), 0, "after still files it");
        // A day at its cap.
        let d = tempfile_dir("gate-cap");
        let (mut s, mut l, t) = asking(&d);
        typed(&mut l, &mut s, &d, "first plan we could try next");
        l.after(&mut s);
        let day = crate::memory::now().chars().take(10).collect::<String>();
        let id = active(&s).unwrap()[0].0.clone();
        mark_asked(&mut s, &id, &day).unwrap();
        l.settings.asks_per_day = 1;
        typed(&mut l, &mut s, &d, "second plan we could try next");
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None);
        assert_eq!(spooled(&d), 1);
        // A record added inside the person's turn does not cost the question.
        let d = tempfile_dir("gate-companion");
        let (mut s, mut l, t) = asking(&d);
        typed(&mut l, &mut s, &d, "a plan and then a skill's body");
        append(&t, r#"{"type":"user","isMeta":true,"turnCompanion":true,"message":{"content":"added"}}"#);
        assert!(l.on_stop(&mut s, "s", false, true, &t).is_some());
    }

    #[test]
    fn options_with_a_comma_in_their_label_are_not_words() {
        let labels = ["Yes, keep it", "No", "Later, maybe, not now"];
        for a in ["No", "Yes, keep it", "Yes, keep it, No", "No, Later, maybe, not now, Yes, keep it", "No, No"] {
            assert!(only_labels(a, &labels), "{a}");
        }
        for a in ["Yes", "Yes, keep", "No, and my own words", "my own words, No", "No,", "No, ", ""] {
            assert!(!only_labels(a, &labels), "{a}");
        }
        // A repeated label is no cost, however long the free text.
        let long = "a, ".repeat(4000) + "words";
        assert!(!only_labels(&long, &["a", "a, a", "a, a, a"]));

        let d = tempfile_dir("comma");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        let q = crate::json!({"questions": [{"question": "Which?", "options": [{"label": "we should keep it, because it costs little to keep"}, {"label": "we should drop it, because it costs too much to keep"}]}], "answers": {"Which?": ["we should keep it, because it costs little to keep", "we should drop it, because it costs too much to keep"]}});
        l.on_ask(&mut s, "s", &q, &Value::Null);
        l.after(&mut s);
        assert!(active(&s).unwrap().is_empty(), "the labels are no person's words");
        // The same words typed as free text are, so the check above is the labels'.
        let q = crate::json!({"questions": [{"question": "Which?", "options": [{"label": "A"}]}], "answers": {"Which?": "we should keep it, because it costs little to keep"}});
        l.on_ask(&mut s, "s", &q, &Value::Null);
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1);
    }

    #[test]
    fn a_long_capture_is_filed_but_never_asked_about() {
        let d = tempfile_dir("long");
        let mut s = store(&d);
        let wide = IdeaSettings { band_below: 100.0, band_above: 100.0, asks_per_day: 9, ..on() };
        let mut l = live(&d, &wide);
        let t = typed(&mut l, &mut s, &d, &"a very long plan ".repeat(60));
        l.after(&mut s);
        assert_eq!(active(&s).unwrap().len(), 1);
        assert_eq!(l.on_stop(&mut s, "s", false, true, &t), None);
    }

    #[test]
    fn prompts_no_person_typed_are_not_filed() {
        let d = tempfile_dir("origin");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        let t = d.join("t.jsonl");
        let line = crate::json!({"type": "user", "isMeta": true, "message": {"content": IDEA}}).to_string();
        let path = t.to_string_lossy().into_owned();
        l.on_prompt(&mut s, "s", IDEA, &path);
        std::fs::write(&t, format!("{line}\n")).unwrap();
        l.after(&mut s);
        assert!(active(&s).unwrap().is_empty());
        assert!(drain(&mut s, &spool_dir(&d.join("memory.sqlite")), 9).unwrap(), "the item is gone");
    }

    #[test]
    fn a_held_capture_waits_then_files_as_its_record_says() {
        let d = tempfile_dir("hold");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        let t = d.join("t.jsonl");
        let path = t.to_string_lossy().into_owned();
        // The hook runs before the record is written: nothing yet, nothing lost.
        l.on_prompt(&mut s, "s", IDEA, &path);
        l.after(&mut s);
        assert!(active(&s).unwrap().is_empty());
        let line = crate::json!({"type": "user", "message": {"content": IDEA}, "origin": {"kind": "human"}}).to_string();
        std::fs::write(&t, format!("{line}\n")).unwrap();
        // Its next try comes round.
        l.retries = Retries::default();
        l.after(&mut s);
        let rows = active(&s).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(meta_value(&rows[0].2, "weights"), Some(l.model.sha.as_str()));
        assert_eq!(l.model.sha.len(), 8);
    }

    #[test]
    fn a_hold_that_runs_out_files_unverified_or_drops() {
        let d = tempfile_dir("expire");
        let mut s = store(&d);
        let dir = spool_dir(&d.join("memory.sqlite"));
        let c = |text: &str| Capture::new(text, "typed", "s", |_: &str| Some((1.0, State::Pending, true)), &[]).unwrap();
        let t = d.join("t.jsonl");
        std::fs::write(&t, "{\"type\":\"user\",\"message\":{\"content\":\"other words\"}}\n").unwrap();
        // The transcript is there and the match missed: kept, out of the surfaces.
        let kept = c("a plan whose record the match missed");
        let old = Hold { transcript: t.to_string_lossy().into_owned(), head: origin::head_hash(&kept.text), since_ms: 1, from: 0 };
        spool(&dir, "capture-a", &kept.to_json(Some(&old))).unwrap();
        // No transcript at all: a headless run's.
        let gone = c("a plan from a run with no transcript");
        let no = Hold { transcript: "Z:/no/such/file.jsonl".into(), head: origin::head_hash(&gone.text), since_ms: 1, from: 0 };
        spool(&dir, "capture-b", &gone.to_json(Some(&no))).unwrap();
        assert!(drain(&mut s, &dir, 9).unwrap());
        let rows = active(&s).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(meta_value(&rows[0].2, "origin"), Some("unverified"));
        assert_eq!(meta_value(&rows[0].2, "state"), Some("unsure"));
        assert_eq!(meta_value(&rows[0].2, "ask"), None);
        assert_eq!(pending_line(&s).unwrap(), None);
    }

    #[test]
    fn a_record_written_before_its_hold_ends_as_unverified() {
        // Should a record ever precede its hook: neither a person's nor dropped.
        let d = tempfile_dir("before-hold");
        let mut s = store(&d);
        let dir = spool_dir(&d.join("memory.sqlite"));
        let c = |text: &str| Capture::new(text, "typed", "s", |_: &str| Some((1.0, State::Pending, true)), &[]).unwrap();
        let t = d.join("t.jsonl");
        for (name, text, extra) in [("capture-a", "a plan a person typed first", r#""origin":{"kind":"human"}"#), ("capture-b", "a plan a peer sent first", r#""isMeta":true,"origin":{"kind":"peer"}"#)] {
            std::fs::write(&t, format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{text}\"}},{extra}}}\n")).unwrap();
            let item = c(text);
            let held = Hold { transcript: t.to_string_lossy().into_owned(), head: origin::head_hash(&item.text), since_ms: 1, from: transcript_len(&t.to_string_lossy()).unwrap() };
            spool(&dir, name, &item.to_json(Some(&held))).unwrap();
            assert!(drain(&mut s, &dir, 9).unwrap());
        }
        let rows = active(&s).unwrap();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(meta_value(&r.2, "origin"), Some("unverified"));
            assert_eq!(meta_value(&r.2, "ask"), None);
        }
        assert_eq!(pending_line(&s).unwrap(), None);
    }

    #[test]
    fn a_spool_item_from_before_the_rule_reads_from_the_start() {
        let c = Capture::new("an item", "typed", "s", |_: &str| Some((1.0, State::Pending, true)), &[]).unwrap();
        let held = Hold { transcript: "t".into(), head: "h".into(), since_ms: 1, from: 7 };
        let v = crate::json::parse(&c.to_json(Some(&held))).unwrap();
        assert_eq!(Capture::hold_of(&v).unwrap().from, 7);
        let old = crate::json!({"held": true, "transcript": "t", "head": "h", "since_ms": 1.0});
        assert_eq!(Capture::hold_of(&old).unwrap().from, 0);
    }

    #[test]
    fn an_answer_comes_back_through_the_marker_and_free_text_is_captured_apart() {
        let d = tempfile_dir("answer");
        let mut s = store(&d);
        let wide = IdeaSettings { band_below: 100.0, band_above: 100.0, asks_per_day: 9, ..on() };
        let mut l = live(&d, &wide);
        typed(&mut l, &mut s, &d, "maybe a plan we could try");
        l.after(&mut s);
        let id = active(&s).unwrap()[0].0.clone();
        mark_asked(&mut s, &id, "2026-01-01").unwrap();
        let q = format!("[idea {id}] Is this a standalone statement?");
        let answer = |a: &str| crate::json::parse(&format!(r#"{{"questions": [{{"question": "{q}", "options": [{{"label": "Yes"}}, {{"label": "No"}}]}}], "answers": {{"{q}": "{a}"}}}}"#)).unwrap();
        // Free text typed into the question: no label, and the words are kept,
        // though the detector alone would not keep them.
        l.on_ask(&mut s, "s", &Value::Null, &answer("half of it, the invented count"));
        l.after(&mut s);
        let rows = active(&s).unwrap();
        assert_eq!(rows.len(), 2);
        let note = rows.iter().find(|r| r.0 != id).unwrap();
        assert_eq!(meta_value(&note.2, "about"), Some(id.as_str()));
        assert_eq!(meta_value(&note.2, "source"), Some("ask"));
        assert_eq!(meta_value(&rows.iter().find(|r| r.0 == id).unwrap().2, "label"), None);
        let note_id = note.0.clone();
        set_label(&mut s, &note_id, false, DISMISS_SOURCE).unwrap();
        l.on_ask(&mut s, "s", &Value::Null, &answer("No"));
        l.after(&mut s);
        assert!(active(&s).unwrap().is_empty());
        let h = s.head(&key_of(&id)).unwrap().unwrap();
        assert_eq!((meta_value(&h.meta, "label"), meta_value(&h.meta, "label_source")), (Some("0"), Some("live")));
        // An option label is no capture; free text the detector likes is.
        let resp = crate::json!({"questions": [{"question": "Which?", "options": [{"label": "A"}, {"label": "B"}]}], "answers": {"Which?": "A"}});
        l.on_ask(&mut s, "s", &Value::Null, &resp);
        l.after(&mut s);
        assert!(active(&s).unwrap().is_empty());
        let resp = crate::json!({"questions": [{"question": "Which?", "options": [{"label": "A"}]}], "answers": {"Which?": IDEA}});
        l.on_ask(&mut s, "s", &Value::Null, &resp);
        l.after(&mut s);
        let rows = active(&s).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(meta_value(&rows[0].2, "source"), Some("ask"));
        assert_eq!(meta_value(&rows[0].2, "ask"), None);
    }

    #[test]
    fn a_surface_shows_only_news_and_stays_bounded() {
        let d = tempfile_dir("line");
        let mut s = store(&d);
        let mut l = live(&d, &on());
        assert_eq!(l.line(&mut s, "s"), None);
        for i in 0..40 {
            let long = "word ".repeat(200);
            let judged = |_: &str| Some((1.0, State::Pending, false));
            let c = Capture::new(&format!("plan {i} {long}"), "typed", "s", judged, &[]).unwrap();
            file(&mut s, &c).unwrap();
        }
        let line = l.line(&mut s, "s").unwrap();
        assert!(line.contains("not requests: 40 in the last 7 days"), "{line}");
        assert!(!line.contains("dismiss"), "{line}");
        assert!(line.len() <= LINE_BYTES + 1, "{}", line.len());
        // Nothing new: nothing said, to this session; another still hears.
        assert_eq!(l.line(&mut s, "s"), None);
        assert!(l.line(&mut s, "other").is_some());
        // An unsure capture is not news.
        let judged = |_: &str| Some((0.0, State::Unsure, true));
        file(&mut s, &Capture::new("an unsure plan", "typed", "s", judged, &[]).unwrap()).unwrap();
        assert_eq!(l.line(&mut s, "s"), None);
        let card = l.resume_line(&s).unwrap();
        assert!(card.contains("not requests: 40, newest "), "{card}");
        assert!(card.len() < 120, "{}", card.len());
    }

    #[test]
    fn a_surface_counts_seven_days_and_older_ideas_stay_pending() {
        let d = tempfile_dir("window");
        let mut s = store(&d);
        let mut w = s.writer().unwrap();
        w.unindexed();
        let old = Draft { title: "an old plan".into(), meta: "state: pending\nfiled: 2020-01-01T00:00:00Z".into(), body: "an old plan".into(), reason: "t".into(), author: "hook".into(), client: "atlas-hook".into(), provenance: None, time: None };
        w.create(&key_of("0123456789"), "journal", ENTRY, SCOPE, &old, None).unwrap();
        w.commit().unwrap();
        // Nothing in the window: nothing on any surface, whatever is pending.
        assert_eq!(pending_line(&s).unwrap(), None);
        let l = live(&d, &on());
        assert_eq!(l.resume_line(&s), None);
        let judged = |_: &str| Some((1.0, State::Pending, false));
        file(&mut s, &Capture::new(IDEA, "typed", "s", judged, &[]).unwrap()).unwrap();
        let (line, _) = pending_line(&s).unwrap().unwrap();
        assert!(line.contains(": 1 in the last 7 days") && !line.contains("an old plan"), "{line}");
        assert_eq!(s.head(&key_of("0123456789")).unwrap().unwrap().status, "active");
    }

    #[test]
    fn it_survives_a_restart() {
        let d = tempfile_dir("restart");
        {
            let mut s = store(&d);
            let mut l = live(&d, &on());
            typed(&mut l, &mut s, &d, IDEA);
            l.after(&mut s);
        }
        let s = store(&d);
        assert_eq!(active(&s).unwrap().len(), 1);
        assert!(pending_line(&s).unwrap().is_some());
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("atlas-ideas-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
