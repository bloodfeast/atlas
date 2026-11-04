//! Deterministic routing in the tool hooks: a large tool result becomes a
//! digest plus a handle. Called from [`crate::hooks`]; every error fails
//! open (the call passes through untouched and a miss is logged).
//!
//! Routing is a PostToolUse swap, not a PreToolUse rewrite. Claude Code
//! checks permission rules against the rewritten command, one subcommand at
//! a time (measured 2026-10-31): rewriting `cat f` to `atlas run -- cat f`
//! would need an allow rule for `atlas run`, which would let any command
//! through. After the tool has run there is nothing left to permit, and
//! Claude Code keeps an over-long Bash output whole on disk
//! (`persistedOutputPath`), so the handle stores the full output.
//!
//! The replacement keeps each tool's output shape (Claude Code ignores a
//! mismatched `updatedToolOutput` for built-in tools), measured for:
//!
//! * Bash: `{stdout, stderr, interrupted, isImage, noOutputExpected}`, plus
//!   `persistedOutputPath` and `persistedOutputSize` past 30 000 characters;
//! * Read: `{type: "text", file: {filePath, content, numLines, startLine,
//!   totalLines}}`;
//! * Grep: `{mode, numFiles, filenames, content, numLines, totalLines}`;
//! * Glob: `{filenames, durationMs, numFiles, truncated, totalMatches,
//!   countIsComplete}`.
//!
//! The answer is a pure function of the event and the stored handles: the
//! same output gives the same handle and digest.
//!
//! # Annotations
//!
//! A result small enough to pass whole is annotated from the code index
//! instead ([`Router::annotate`]), so a guessed window becomes a known one:
//!
//! * each Grep content match line gets its enclosing item and range
//!   appended, `src/a.rs:12:    let x = y; [fn Store::open 8-30]`
//!   (context lines and line counts are unchanged);
//! * a Read window that cuts an item gets one `additionalContext` line
//!   naming the cut items with their ranges; the content is untouched, so
//!   line numbers and `Edit` strings stay exact;
//! * shell reads get the same, chosen from the command as written
//!   ([`shell_read`]): `grep -n` and `rg -n` output (and PowerShell's
//!   `Select-String`, whose lines are `path:line:text`) gets the Grep
//!   suffix on its `stdout`, and a window (`sed -n 'A,Bp' F`, `cat -n F |
//!   sed -n ...`, `head`/`tail -n N F`, and `Get-Content` with
//!   `-TotalCount`, `-Tail`, `| Select-Object -Skip S -First N` or
//!   `(Get-Content F)[A..B]`) gets the Read note. A Bash batch of reads
//!   separated by `echo TEXT` lines is read part by part ([`segments`]).
//!   Any other command, including a chained or redirected one, passes as
//!   it is.
//!
//! A file no code root holds, or one written after its root was indexed,
//! is not annotated.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::handles::{HandleCfg, Handles, Meta};
use crate::hooks::HookEvent;
use crate::json::Value;
use crate::memory::settings::Settings;

/// The file whose presence turns routing off: beside the index database,
/// or in the user-level state folder of a configuration with projects (one
/// switch for every project; see [`crate::project::user_state_dir`]).
pub const OFF_FLAG: &str = "routing-off";
/// A Bash command containing this passes through unrouted.
pub const RAW_MARK: &str = "ATLAS_RAW=1";
/// The `permission_mode` in which PreToolUse appends no exit tail (see
/// [`Router::pre_tool_use`]).
pub const AUTO_MODE: &str = "auto";

/// The routing off-switch file for an index database.
pub fn off_flag(db: &Path) -> PathBuf {
    off_flag_in(db.parent().unwrap_or(Path::new(".")))
}

/// The routing off-switch file in a state folder.
pub fn off_flag_in(dir: &Path) -> PathBuf {
    dir.join(OFF_FLAG)
}

/// Turns routing on or off (`atlas settings off routing` and `set routing
/// on`). Takes effect on the next tool call.
///
/// # Errors
///
/// [`Error::Io`] writing or removing the flag file.
pub fn set_routing(db: &Path, on: bool) -> Result<()> {
    set_routing_in(db.parent().unwrap_or(Path::new(".")), on)
}

/// [`set_routing`] with the flag in a state folder.
///
/// # Errors
///
/// [`Error::Io`] writing or removing the flag file.
pub fn set_routing_in(dir: &Path, on: bool) -> Result<()> {
    let f = off_flag_in(dir);
    if on {
        match std::fs::remove_file(&f) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(format!("{}: {e}", f.display()))),
        }
    } else {
        if let Some(p) = f.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(&f, b"routing off: tool results pass through whole\n").map_err(|e| Error::Io(format!("{}: {e}", f.display())))
    }
}

/// Whether routing is on: the configuration's `lean.routing` (default
/// true) and no off-switch file.
pub fn routing_on(db: &Path, lean: Option<&Value>) -> bool {
    routing_on_in(db.parent().unwrap_or(Path::new(".")), lean)
}

/// [`routing_on`] with the flag in a state folder.
pub fn routing_on_in(dir: &Path, lean: Option<&Value>) -> bool {
    let cfg = lean.and_then(|l| l.get("routing")).and_then(Value::as_bool).unwrap_or(true);
    cfg && !off_flag_in(dir).exists()
}

/// The marker as a literal, for [`EXIT_MARK`] and [`OR_ECHO`].
macro_rules! exit_mark {
    () => {
        "atlas-exit-nonzero"
    };
}

/// The line a rewritten one-line command prints when it exits nonzero.
pub const EXIT_MARK: &str = exit_mark!();

/// The Bash exit tail.
const OR_ECHO: &str = concat!(" || echo ", exit_mark!());

/// The PowerShell tail after a cargo build or test: a read-only native
/// command that prints nothing and exits 0 (see [`rewrite`]).
pub const PS_RESET: &str = "where.exe /Q where.exe";

/// The command words whose output is routed (R10: shell reads and
/// searches, and cargo builds and tests). `git` is routed only for the
/// subcommands in [`GIT_READS`].
const ROUTED: &[&str] = &[
    "cat", "sed", "head", "tail", "grep", "rg", "find", "ls", "tree", "wc", "nl", "Get-Content", "gc", "Select-String", "sls",
    "Get-ChildItem", "gci", "dir", "type",
];

/// The git subcommands that only read. Any other subcommand (push, commit,
/// merge, an alias) runs untailed, so its exit status reaches the caller
/// as it is (issue bloodfeast/atlas#3).
const GIT_READS: &[&str] = &[
    "log", "diff", "show", "status", "blame", "annotate", "grep", "ls-files", "ls-tree", "ls-remote", "rev-parse", "rev-list", "shortlog",
    "describe", "cat-file", "name-rev", "merge-base", "for-each-ref", "show-ref", "diff-tree", "diff-index", "diff-files", "range-diff",
    "whatchanged", "count-objects",
];

/// The git global options that take their value as the next word.
const GIT_VALUE_OPTS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--config-env", "--attr-source", "--shallow-file"];

/// The command word of one step, without its path or `.exe`.
fn command_word(step: &str) -> &str {
    let first = step.split_whitespace().next().unwrap_or("");
    first.rsplit(['/', '\\']).next().unwrap_or(first).trim_end_matches(".exe")
}

/// Whether the words after `git` run a read-only subcommand, skipping the
/// global options first (`-C dir`, `-c k=v`, `--no-pager`, `--git-dir=x`).
fn git_reads(mut rest: &str) -> bool {
    while let Some((w, r)) = next_word(rest) {
        rest = r;
        if GIT_VALUE_OPTS.contains(&w) {
            rest = next_word(rest).map_or("", |(_, r)| r);
        } else if !w.starts_with('-') {
            return GIT_READS.contains(&w);
        }
    }
    false
}

/// A byte that ends a word inside a piece of a command.
fn word_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '!' | '{' | '}' | '<' | '>')
}

/// The next word of a piece, a quoted span kept whole and its outer quotes
/// dropped, and the rest of the piece. A backslash outside quotes keeps the
/// next character in the word, as the shell does (`a\ log` is one word).
fn next_word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start_matches(word_sep);
    if s.is_empty() {
        return None;
    }
    let mut quote = None;
    let mut end = s.len();
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '\\' => escaped = true,
            None if word_sep(c) => {
                end = i;
                break;
            }
            None => {}
        }
    }
    Some((s[..end].trim_matches(['"', '\'']), &s[end..]))
}

/// Whether a `git` anywhere in one piece runs other than to read. Every
/// word is checked, not only the first, so git behind `sudo`, `env`,
/// `time`, `xargs` or `!` is caught, and a quoted word is checked as a
/// command of its own (`bash -c "git push"`). A quoted value stays one
/// word, so `-C "a log"` cannot pass for a subcommand.
fn piece_writes(mut rest: &str) -> bool {
    while let Some((w, r)) = next_word(rest) {
        rest = r;
        if (w.contains(char::is_whitespace) && piece_writes(w)) || (command_word(w).eq_ignore_ascii_case("git") && !git_reads(r)) {
            return true;
        }
    }
    false
}

/// Whether a `git` anywhere in the command runs other than to read: the
/// tail follows the whole command, so it would hide that git's failure.
/// The command is cut into pieces at chain, pipe and substitution
/// boundaries first, so an option value never eats an inner command
/// (`git -C "$(git rev-parse --show-toplevel)" push`); the cost is that the
/// same form with a read is unrouted too. A `git` in prose
/// (`grep "git push"`) reads as a write and only loses its digest.
fn git_writes(cmd: &str) -> bool {
    cmd.split(['&', '|', ';', '(', ')', '`', '$', '\n']).any(piece_writes)
}

/// Whether a shell command is one routing rewrites: its first command word
/// (after `cd ... &&` and environment assignments) is a reader or search,
/// `git` reading, or `cargo` building or testing; and no piece of it runs
/// git to write.
pub fn routable(cmd: &str) -> bool {
    let step = crate::digest::last_step(cmd);
    let first = command_word(step);
    let routed = if first == "cargo" {
        let sub = step.split_whitespace().skip(1).find(|w| !w.starts_with('+') && !w.starts_with('-')).unwrap_or("");
        matches!(sub, "build" | "b" | "test" | "t" | "check" | "c" | "clippy" | "nextest")
    } else {
        // A first word of `git` is routed only because git_writes below
        // refuses every git that is not a read.
        first.eq_ignore_ascii_case("git") || ROUTED.iter().any(|w| w.eq_ignore_ascii_case(first))
    };
    routed && !git_writes(cmd)
}

/// The `additionalContext` line for a tailed command that printed
/// [`EXIT_MARK`]. Only for a result routing failed on
/// ([`Router::post_tool_use`] erred), which reaches the model raw; a
/// routed result already says `Exit code nonzero` or `exit nonzero`.
pub fn exit_note(ev: &HookEvent) -> Option<&'static str> {
    let stdout = s(&ev.tool_response, "stdout");
    (ev.tool_name == "Bash" && unwrite(s(&ev.tool_input, "command"), stdout).1 == Some(-1)).then_some("atlas: exit nonzero")
}

/// The routed form of a command (see [`Router::pre_tool_use`]).
///
/// `None` for a Bash command a tail cannot follow without losing its exit
/// status: several lines, a comment or a here-document. (A brace group,
/// `{ CMD\n} || echo ...`, would keep it, but Claude Code refuses a brace
/// group before it runs: measured.) Such a command runs untailed, and a
/// large failing output reaches the model raw rather than with its failure
/// hidden.
pub fn rewrite(shell: &str, cmd: &str) -> Option<String> {
    let cmd = cmd.trim_end();
    if shell == "PowerShell" {
        // No tail can carry a native exit code here: PowerShell 5.1 has no
        // `||`, the permission check refuses every `$` (`$LASTEXITCODE`,
        // `$?`), `exit`, `cmd /c` and `Get-Variable` or `variable:` reads,
        // and Claude Code reports failure from the last native command's
        // exit code (measured, 26 candidates; see docs/ARCHITECTURE.md). A
        // trailing read-only native command that exits 0 does pass, and makes
        // the call a PostToolUse. That loses the status, so it is used only
        // for cargo builds and tests, whose output states the result (the
        // digest infers the verdict and keeps the tail verbatim), and only on
        // Windows, where `where.exe` is always present. Every other command
        // keeps the inert `Write-Output ''` and a nonzero exit passes raw, so
        // a silent failure is never hidden.
        if cfg!(windows) && matches!(crate::digest::shape("bash", cmd, ""), crate::digest::Shape::CargoTest | crate::digest::Shape::CargoBuild) {
            return Some(format!("{cmd}\n{PS_RESET}"));
        }
        return Some(format!("{cmd}\nWrite-Output ''"));
    }
    (!cmd.contains('\n') && !cmd.contains('#') && !cmd.contains("<<")).then(|| format!("{cmd}{OR_ECHO}"))
}

/// The command as the model wrote it, and what the tail says of its exit:
/// `Some(0)`, `Some(-1)` (nonzero, code unknown), or `None` (no tail, or
/// one that cannot tell).
pub fn unwrite<'a>(cmd: &'a str, stdout: &str) -> (&'a str, Option<i64>) {
    if let Some(c) = cmd.strip_suffix(OR_ECHO) {
        let failed = marked(stdout);
        return (c, Some(if failed { -1 } else { 0 }));
    }
    if let Some(c) = cmd.strip_suffix("\ntrue").or_else(|| cmd.strip_suffix("\nWrite-Output ''")) {
        return (c, None);
    }
    if let Some(c) = cmd.strip_suffix(PS_RESET).and_then(|c| c.strip_suffix('\n')) {
        return (c, None);
    }
    (cmd, None)
}

/// A path shown relative to the working directory when it is inside it
/// (compared with `/` separators, ignoring ASCII case on Windows).
pub fn relative(path: &str, cwd: &str) -> String {
    let p = path.replace('\\', "/");
    let c = cwd.replace('\\', "/");
    let c = c.trim_end_matches('/');
    if c.is_empty() || p.len() <= c.len() + 1 || !p.is_char_boundary(c.len()) {
        return p;
    }
    let (head, rest) = p.split_at(c.len());
    let same = if cfg!(windows) { head.eq_ignore_ascii_case(c) } else { head == c };
    if same && rest.starts_with('/') {
        rest[1..].to_string()
    } else {
        p
    }
}

/// Output without the exit marker line.
///
/// Only an exact final line is the marker: a command whose own output
/// mentions it keeps that text.
fn strip_marker(stdout: &str) -> &str {
    let t = stdout.trim_end_matches(['\n', '\r']);
    if t == EXIT_MARK {
        return "";
    }
    match t.strip_suffix(EXIT_MARK) {
        Some(rest) if rest.ends_with('\n') => rest.trim_end_matches(['\n', '\r']),
        _ => stdout,
    }
}

/// Whether the output's final line is exactly the marker.
fn marked(stdout: &str) -> bool {
    strip_marker(stdout).len() != stdout.len()
}

/// The routing state a hook worker keeps between events.
pub struct Router {
    db: PathBuf,
    /// The folder holding the routing off-switch.
    flags: PathBuf,
    lean: Option<Value>,
    cfg: HandleCfg,
    handles: Option<Handles>,
    /// The connection R7 re-indexing writes through.
    code_conn: Option<rusqlite::Connection>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router").field("db", &self.db).field("cfg", &self.cfg).finish()
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// A reader command that names its own window (`sed -n A,Bp`, `head -n`,
/// `tail -n`): the model chose a range, so a larger output passes.
fn windowed(cmd: &str) -> bool {
    let c = cmd.trim_start();
    (c.starts_with("sed ") && c.contains(" -n")) || ((c.starts_with("head ") || c.starts_with("tail ")) && (c.contains(" -n") || c.contains(" -")))
}

impl Router {
    /// The router for these settings.
    pub fn new(settings: &Settings) -> Router {
        let flags = settings.user_state.clone().unwrap_or_else(|| settings.index_db.parent().unwrap_or(Path::new(".")).to_path_buf());
        Router { db: settings.index_db.clone(), flags, lean: settings.lean.clone(), cfg: HandleCfg::from_lean(settings.lean.as_ref()), handles: None, code_conn: None }
    }

    /// Whether routing is on now (the configuration and the off-switch).
    pub fn routing_on(&self) -> bool {
        routing_on_in(&self.flags, self.lean.as_ref())
    }

    /// A router over an index database (tests and `atlas run`).
    pub fn with_db(db: &Path, lean: Option<Value>) -> Router {
        let cfg = HandleCfg::from_lean(lean.as_ref());
        let flags = db.parent().unwrap_or(Path::new(".")).to_path_buf();
        Router { db: db.to_path_buf(), flags, lean, cfg, handles: None, code_conn: None }
    }

    fn handles(&mut self) -> Result<&mut Handles> {
        if self.handles.is_none() {
            let h = Handles::open(&self.db, self.cfg)?;
            let _ = h.conn().execute_batch(crate::tokens::TOKEN_TABLE);
            self.handles = Some(h);
        }
        self.handles.as_mut().ok_or_else(|| Error::Db("handle store".into()))
    }

    /// PreToolUse: the replacement `tool_input` (Claude Code's
    /// `updatedInput`), or `None` to leave the call as it is.
    ///
    /// A failing command fires PostToolUseFailure, whose output cannot be
    /// replaced (measured 2026-10-31), so a routed shell command gets a
    /// tail that makes it exit 0 while keeping the failure visible:
    /// `CMD || echo atlas-exit-nonzero` on one line, else `CMD` then `true`
    /// (Bash) or `Write-Output ''` (PowerShell) on a line of their own. The
    /// command itself is untouched, so the user's permission rules still
    /// decide it; the tails are read-only and need no rule.
    ///
    /// In auto mode nothing is appended. The auto-mode classifier judges the
    /// command the model wrote, and no hook output makes it judge the
    /// rewritten one: Claude Code checks permission rules against
    /// `updatedInput`, but the classifier's only hook channel is PostToolUse
    /// `classifierContext`, which is about results. With a tail, the
    /// classifier refused every routed command once. Untailed, a successful
    /// command still reaches PostToolUse and is digested; a failing one
    /// arrives as Claude Code's own error text.
    ///
    /// An input without `permission_mode` keeps the tail. The field is a
    /// documented common input, so its absence means a client that reports
    /// no mode, and the tail is a read-only `echo` that never widens what
    /// runs: keeping it costs at most a resend, dropping it loses failures.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn pre_tool_use(&mut self, ev: &HookEvent) -> Result<Option<Value>> {
        if !matches!(ev.tool_name.as_str(), "Bash" | "PowerShell") || !self.routing_on() || ev.permission_mode == AUTO_MODE {
            return Ok(None);
        }
        let input = &ev.tool_input;
        let cmd = s(input, "command");
        if cmd.trim().is_empty()
            || cmd.contains(RAW_MARK)
            || cmd.contains(EXIT_MARK)
            || input.get("run_in_background").and_then(Value::as_bool) == Some(true)
            || !routable(cmd)
        {
            return Ok(None);
        }
        let Some(new) = rewrite(&ev.tool_name, cmd) else { return Ok(None) };
        let mut ti = input.clone();
        let o = ti.as_object_mut().ok_or_else(|| Error::InvalidArgument("tool_input".into()))?;
        o.insert("command".into(), Value::from(new));
        Ok(Some(ti))
    }

    /// PostToolUse: the replacement tool output in the tool's own shape, or
    /// `None` to leave the result as it is (small, unrouted, routing off).
    ///
    /// # Errors
    ///
    /// Any failure; the caller logs it and passes the result through.
    pub fn post_tool_use(&mut self, ev: &HookEvent) -> Result<Option<Value>> {
        if !self.routing_on() || ev.tool_response.as_object().is_none() {
            return Ok(None);
        }
        let session = if ev.agent_id.is_empty() { ev.session_id.clone() } else { ev.agent_id.clone() };
        let th = self.cfg.threshold_bytes;
        let resp = &ev.tool_response;
        let input = &ev.tool_input;
        let (full, meta, extra_stderr) = match ev.tool_name.as_str() {
            "Bash" | "PowerShell" => {
                let (cmd, exit) = unwrite(s(input, "command"), s(resp, "stdout"));
                let c = cmd.trim_start();
                if cmd.contains(RAW_MARK) || c.starts_with("atlas slice") || c.starts_with("atlas run") {
                    return Ok(None);
                }
                if resp.get("isImage").and_then(Value::as_bool) == Some(true) {
                    return Ok(None);
                }
                let persisted = s(resp, "persistedOutputPath");
                let stdout = match persisted {
                    "" => s(resp, "stdout").to_string(),
                    p => std::fs::read(p).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|_| s(resp, "stdout").to_string()),
                };
                let stdout = strip_marker(&stdout).to_string();
                let stderr = s(resp, "stderr");
                // Build and test output digests well at any size; a read the
                // model windowed itself gets more room.
                let build = matches!(crate::digest::shape("bash", c, ""), crate::digest::Shape::CargoTest | crate::digest::Shape::CargoBuild);
                let limit = if build { th.min(self.cfg.build_threshold_bytes) } else if windowed(c) { th * 3 } else { th };
                if stdout.len() + stderr.len() < limit {
                    // Small: whole, with a failure said the way Claude Code says it.
                    if exit == Some(-1) {
                        let mut out = resp.clone();
                        if let Some(o) = out.as_object_mut() {
                            o.insert("stdout".into(), Value::from(format!("Exit code nonzero\n{stdout}")));
                        }
                        return Ok(Some(out));
                    }
                    return Ok(None);
                }
                // A short stderr stays visible as it is; a long one goes into the handle.
                let (full, keep_err) = if stderr.len() > 2000 { (format!("{stdout}\n{stderr}"), false) } else { (stdout, true) };
                let kind = if ev.tool_name == "Bash" { "bash" } else { "powershell" };
                let m = Meta { kind: kind.into(), label: cmd.lines().next().unwrap_or("").to_string(), cmd: cmd.into(), cwd: ev.cwd.clone(), session, exit_code: exit, ..Meta::default() };
                (full, m, keep_err)
            }
            "Read" => {
                let file = resp.get("file").cloned().unwrap_or(Value::Null);
                if s(resp, "type") != "text" {
                    return Ok(None);
                }
                // A denied file (.env, keys, credentials) is never kept as a
                // handle or read again from disk, whoever read it first.
                if crate::sources::path_denied(std::path::Path::new(s(&file, "filePath"))) {
                    return Ok(None);
                }
                let windowed = input.get("limit").is_some() || input.get("offset").is_some();
                let mut content = s(&file, "content").to_string();
                // A whole-file Read can come back cut short (Claude Code caps
                // it); the handle keeps the whole file so the outline and
                // every slice cover all of it.
                let total = file.get("totalLines").and_then(Value::as_u64).unwrap_or(0);
                let shown = file.get("numLines").and_then(Value::as_u64).unwrap_or(0);
                if !windowed && total > shown {
                    let p = s(&file, "filePath");
                    if let Ok(b) = std::fs::read(p) {
                        if b.len() <= crate::handles::MAX_CAPTURE {
                            content = String::from_utf8_lossy(&b).into_owned();
                        }
                    }
                }
                let limit = if windowed { th * 3 } else { th };
                if content.len() < limit {
                    return Ok(None);
                }
                let path = s(&file, "filePath").to_string();
                let start = file.get("startLine").and_then(Value::as_u64).unwrap_or(1);
                let shown = relative(&path, &ev.cwd);
                let label = if start > 1 { format!("{shown} from line {start}") } else { shown };
                (content, Meta { kind: "read".into(), label, cmd: path, cwd: ev.cwd.clone(), session, ..Meta::default() }, true)
            }
            "Grep" => {
                let mode = s(resp, "mode");
                let pattern = s(input, "pattern");
                let (text, kind) = if mode == "content" {
                    (s(resp, "content").to_string(), "grep")
                } else {
                    let names: Vec<&str> = resp.get("filenames").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    (names.join("\n"), "files")
                };
                if text.len() < th {
                    return Ok(None);
                }
                let label = format!("Grep {pattern:?} {}", s(input, "path"));
                (text, Meta { kind: kind.into(), label, cmd: format!("grep {pattern} {} {}", s(input, "path"), s(input, "glob")), cwd: ev.cwd.clone(), session, ..Meta::default() }, true)
            }
            "Glob" => {
                let names: Vec<&str> = resp.get("filenames").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let text = names.join("\n");
                if text.len() < th {
                    return Ok(None);
                }
                let label = format!("Glob {:?} {}", s(input, "pattern"), s(input, "path"));
                (text, Meta { kind: "glob".into(), label, cmd: format!("glob {} {}", s(input, "pattern"), s(input, "path")), cwd: ev.cwd.clone(), session, ..Meta::default() }, true)
            }
            _ => return Ok(None),
        };
        let h = self.handles()?;
        let row = h.put(full.as_bytes(), &meta)?;
        let shape = Handles::shape_of(&row, &full);
        let digest = crate::digest::digest(&row.id, &row.meta.label, shape, &full, meta.exit_code.map(|c| c as i32), h.cfg.digest_bytes);
        let _ = crate::tokens::log_row(
            h.conn(),
            &format!("route:{}", ev.tool_name),
            digest.len(),
            Some(crate::tokens::Counterfactual { grep: 0, read: full.len() }),
        );
        let mut out = resp.clone();
        let o = out.as_object_mut().ok_or_else(|| Error::InvalidArgument("tool_response".into()))?;
        match ev.tool_name.as_str() {
            "Bash" | "PowerShell" => {
                o.insert("stdout".into(), Value::from(digest));
                if !extra_stderr {
                    o.insert("stderr".into(), Value::from(""));
                }
                o.remove("persistedOutputPath");
                o.remove("persistedOutputSize");
            }
            "Read" => {
                let n = digest.lines().count() as u64;
                if let Some(f) = o.get_mut("file").and_then(Value::as_object_mut) {
                    f.insert("content".into(), Value::from(digest));
                    f.insert("numLines".into(), Value::from(n));
                    f.insert("startLine".into(), Value::from(1u64));
                }
            }
            "Grep" if s(resp, "mode") == "content" => {
                let n = digest.lines().count() as u64;
                o.insert("content".into(), Value::from(digest));
                o.insert("numLines".into(), Value::from(n));
            }
            _ => {
                let lines: Vec<Value> = digest.lines().map(Value::from).collect();
                o.insert("filenames".into(), Value::Array(lines));
            }
        }
        Ok(Some(out))
    }

    /// Annotations for a result that passes whole (see the module docs):
    /// the replacement output for a Grep, the context line for a Read.
    ///
    /// # Errors
    ///
    /// Any failure; the caller logs it and passes the result through.
    pub fn annotate(&mut self, ev: &HookEvent) -> Result<(Option<Value>, String)> {
        if !self.routing_on() || self.lean.as_ref().and_then(|l| l.get("annotate")).and_then(Value::as_bool) == Some(false) {
            return Ok((None, String::new()));
        }
        let resp = &ev.tool_response;
        match ev.tool_name.as_str() {
            "Grep" if s(resp, "mode") == "content" => {
                let content = s(resp, "content");
                if content.is_empty() {
                    return Ok((None, String::new()));
                }
                let input_path = s(&ev.tool_input, "path");
                let single = !input_path.is_empty() && Path::new(input_path).is_file();
                let conn = self.handles()?.conn();
                let out = annotate_grep(content, &ev.cwd, single.then_some(input_path), &mut |p| crate::code::file_items(conn, p))?;
                if out.len() == content.len() {
                    return Ok((None, String::new()));
                }
                let mut r = resp.clone();
                if let Some(o) = r.as_object_mut() {
                    o.insert("content".into(), Value::from(out));
                }
                Ok((Some(r), String::new()))
            }
            "Read" if s(resp, "type") == "text" => {
                let file = resp.get("file").cloned().unwrap_or(Value::Null);
                let start = file.get("startLine").and_then(Value::as_u64).unwrap_or(1) as u32;
                let shown = file.get("numLines").and_then(Value::as_u64).unwrap_or(0) as u32;
                let total = file.get("totalLines").and_then(Value::as_u64).unwrap_or(0) as u32;
                if shown == 0 || (start <= 1 && shown >= total) {
                    return Ok((None, String::new()));
                }
                let path = s(&file, "filePath").to_string();
                let conn = self.handles()?.conn();
                let Some(items) = crate::code::file_items(conn, &path)? else { return Ok((None, String::new())) };
                Ok((None, read_note(&relative(&path, &ev.cwd), start, start + shown - 1, &items)))
            }
            "Bash" | "PowerShell" => {
                let (cmd, _) = unwrite(s(&ev.tool_input, "command"), s(resp, "stdout"));
                let stdout = s(resp, "stdout");
                if stdout.trim().is_empty() || cmd.contains(RAW_MARK) || !s(resp, "persistedOutputPath").is_empty() || resp.get("isImage").and_then(Value::as_bool) == Some(true) {
                    return Ok((None, String::new()));
                }
                let Some(segs) = segments(&ev.tool_name, cmd, stdout, &ev.cwd) else { return Ok((None, String::new())) };
                let lines: Vec<&str> = stdout.split('\n').collect();
                let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
                let mut notes: Vec<String> = Vec::new();
                let conn = self.handles()?.conn();
                for seg in segs {
                    let text = lines[seg.from..seg.to].join("\n");
                    match seg.read {
                        ShellRead::Window { file, from, to, tail } => {
                            let abs = absolute(&file, &seg.dir);
                            if crate::sources::path_denied(std::path::Path::new(&abs)) {
                                continue;
                            }
                            let printed = text.trim_end_matches(['\n', '\r']).lines().count() as u32;
                            if printed == 0 {
                                continue;
                            }
                            let (a, b) = match (to, tail) {
                                (Some(b), _) => (from, b.min(from + printed - 1)),
                                (None, Some(n)) => {
                                    let Ok(t) = std::fs::read(&abs) else { continue };
                                    let total = t.split(|&c| c == b'\n').count() as u32 - u32::from(t.ends_with(b"\n"));
                                    (total.saturating_sub(n.min(printed)) + 1, total)
                                }
                                (None, None) => continue,
                            };
                            let Some(items) = crate::code::file_items(conn, &abs)? else { continue };
                            let note = read_note(&relative(&abs, &ev.cwd), a, b, &items);
                            if !note.is_empty() {
                                notes.push(note);
                            }
                        }
                        ShellRead::Grep { single } => {
                            let single = single.map(|f| absolute(&f, &seg.dir)).filter(|f| Path::new(f).is_file());
                            let done = annotate_grep(&text, &seg.dir, single.as_deref(), &mut |p| crate::code::file_items(conn, p))?;
                            for (k, l) in done.split('\n').enumerate() {
                                out[seg.from + k] = l.to_string();
                            }
                        }
                    }
                }
                let joined = out.join("\n");
                let updated = (joined.len() != stdout.len()).then(|| {
                    let mut r = resp.clone();
                    if let Some(o) = r.as_object_mut() {
                        o.insert("stdout".into(), Value::from(joined));
                    }
                    r
                });
                Ok((updated, notes.join("\n")))
            }
            _ => Ok((None, String::new())),
        }
    }

    /// Re-indexes one file of a code root at once (R7, after an Edit or
    /// Write), on a connection of its own to the index database.
    ///
    /// # Errors
    ///
    /// [`Error::Db`] from SQLite; the file's rows are unchanged then.
    pub fn reindex_code(&mut self, path: &str) -> Result<crate::code::Reindexed> {
        if self.code_conn.is_none() {
            let c = rusqlite::Connection::open(&self.db)?;
            c.busy_timeout(std::time::Duration::from_secs(10))?;
            self.code_conn = Some(c);
        }
        let c = self.code_conn.as_mut().ok_or_else(|| Error::Db("index connection".into()))?;
        crate::code::reindex_file(c, path)
    }

    /// Records a failure of hook work that is not a tool call's routing
    /// (a re-index) as a miss.
    pub fn miss_text(&mut self, query: &str, e: &crate::Error) {
        if let Ok(h) = self.handles() {
            let _ = h.conn().execute(
                "INSERT INTO misses(time, tool, query, returned, found, reason, client) VALUES(?1, 'route', ?2, '', '', ?3, 'atlas-hook')",
                rusqlite::params![crate::store::utc(std::time::SystemTime::now()), query, e.to_string()],
            );
        }
    }

    /// Records a routing failure as a miss; the call already passed
    /// through untouched.
    pub fn miss(&mut self, ev: &HookEvent, e: &crate::Error) {
        eprintln!("atlas routing {} {}: {e}", ev.event, ev.tool_name);
        let query = format!("{} {}", ev.tool_name, s(&ev.tool_input, "command"));
        if let Ok(h) = self.handles() {
            let _ = h.conn().execute(
                "INSERT INTO misses(time, tool, query, returned, found, reason, client) VALUES(?1, 'route', ?2, '', '', ?3, 'atlas-hook')",
                rusqlite::params![crate::store::utc(std::time::SystemTime::now()), query, e.to_string()],
            );
        }
    }
}

/// A shell command whose output annotations can map back to source lines
/// (see [`shell_read`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRead {
    /// It prints lines `from..=to` of one file (`sed -n 'A,Bp' F`, `head`,
    /// `tail`, `Get-Content` windows); `to` is `None` for "to the end".
    Window {
        /// The file as written in the command.
        file: String,
        /// First line, 1-based.
        from: u32,
        /// Last line, or `None` for the end of the file.
        to: Option<u32>,
        /// `tail -n N`: the window is the last `N` lines.
        tail: Option<u32>,
    },
    /// It prints grep-style `path:line:text` matches (`grep -n`, `rg -n`,
    /// `Select-String`), or `line:text` when it searched one named file.
    Grep {
        /// The one file searched, when the output carries no path.
        single: Option<String>,
    },
}

/// Shell words: whitespace-separated, with `'...'` and `"..."` quoting
/// (quotes removed, no escapes or expansion), or `None` when a quote does
/// not close.
fn shell_words(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut q: Option<char> = None;
    for c in s.chars() {
        match q {
            Some(x) if c == x => q = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                q = Some(c);
                has = true;
            }
            None if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => cur.push(c),
        }
    }
    if q.is_some() {
        return None;
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

/// The stages of a pipeline, split on `|` outside quotes; `None` when the
/// command chains or redirects (`;`, `&`, `||`, `>`, `<`, backticks, `$(`),
/// so only one plain pipeline is read.
fn pipeline(cmd: &str) -> Option<Vec<&str>> {
    let b = cmd.as_bytes();
    let mut q: Option<u8> = None;
    let mut cuts = vec![0usize];
    for (i, &c) in b.iter().enumerate() {
        match q {
            Some(x) if c == x => q = None,
            Some(_) => {}
            None => match c {
                b'\'' | b'"' => q = Some(c),
                b';' | b'&' | b'>' | b'<' | b'`' | b'\n' => return None,
                b'$' if b.get(i + 1) == Some(&b'(') => return None,
                b'|' if b.get(i + 1) == Some(&b'|') || (i > 0 && b[i - 1] == b'|') => return None,
                b'|' => cuts.push(i + 1),
                _ => {}
            },
        }
    }
    let mut out = Vec::new();
    for (k, &start) in cuts.iter().enumerate() {
        let end = cuts.get(k + 1).map_or(b.len(), |&e| e - 1);
        out.push(cmd[start..end].trim());
    }
    Some(out)
}

/// `N` from `-n N`, `-nN`, `-N` or `--lines=N` among a head or tail's words.
fn count_arg(words: &[String]) -> Option<(u32, Vec<String>)> {
    let mut n = None;
    let mut rest = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let w = &words[i];
        if w == "-n" || w == "--lines" {
            n = words.get(i + 1).and_then(|v| v.parse().ok());
            i += 2;
            continue;
        }
        if let Some(v) = w.strip_prefix("--lines=").or_else(|| w.strip_prefix("-n")).or_else(|| w.strip_prefix('-')) {
            n = Some(v.parse().ok()?);
        } else {
            rest.push(w.clone());
        }
        i += 1;
    }
    Some((n?, rest))
}

/// A `sed -n 'A,Bp'` (or `'Ap'`) stage: the window and the file it names,
/// if any.
fn sed_window(words: &[String]) -> Option<(u32, u32, Option<String>)> {
    if words.first().map(String::as_str) != Some("sed") || !words.iter().any(|w| w == "-n") {
        return None;
    }
    let rest: Vec<&String> = words[1..].iter().filter(|w| *w != "-n").collect();
    let script = rest.first()?.replace(' ', "");
    let body = script.strip_suffix('p')?;
    let (a, b) = match body.split_once(',') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        None => {
            let a: u32 = body.parse().ok()?;
            (a, a)
        }
    };
    if a == 0 || b < a || rest.len() > 2 {
        return None;
    }
    Some((a, b, rest.get(1).map(|s| s.to_string())))
}

/// Whether a stage after a search only cuts its lines (`head`, `tail`).
fn cuts_only(stage: &str) -> bool {
    let w = stage.split_whitespace().next().unwrap_or("");
    matches!(w, "head" | "tail" | "Select-Object" | "select")
}

/// What a shell command's output is, for annotation: a window of one file
/// or grep-style matches, or `None` for anything else. The command is
/// read as written (after the routing tail is removed); a chained,
/// redirected or substituted command is not read.
pub fn shell_read(shell: &str, cmd: &str) -> Option<ShellRead> {
    // Discarding or merging stderr does not change what stdout is.
    let step = crate::digest::last_step(cmd).replace(" 2>/dev/null", "").replace(" 2>&1", "").replace(" 2>$null", "");
    let stages = pipeline(&step)?;
    let w0 = shell_words(stages[0])?;
    let first = w0.first()?.as_str();
    if shell == "PowerShell" {
        return ps_read(&stages, &w0);
    }
    match first {
        "sed" if stages.len() == 1 => {
            let (a, b, f) = sed_window(&w0)?;
            Some(ShellRead::Window { file: f?, from: a, to: Some(b), tail: None })
        }
        // `cat -n F | sed -n 'A,Bp'`, `nl F | sed -n ...`
        "cat" | "nl" if stages.len() == 2 => {
            let files: Vec<&String> = w0[1..].iter().filter(|w| !w.starts_with('-')).collect();
            let (a, b, f) = sed_window(&shell_words(stages[1])?)?;
            (files.len() == 1 && f.is_none()).then(|| ShellRead::Window { file: files[0].clone(), from: a, to: Some(b), tail: None })
        }
        "head" | "tail" if stages.len() == 1 => {
            let (n, rest) = count_arg(&w0)?;
            if rest.len() != 1 || rest[0].starts_with('-') || n == 0 {
                return None;
            }
            let file = rest[0].clone();
            Some(if first == "head" { ShellRead::Window { file, from: 1, to: Some(n), tail: None } } else { ShellRead::Window { file, from: 1, to: None, tail: Some(n) } })
        }
        "grep" | "egrep" | "rg" if stages[1..].iter().all(|s| cuts_only(s)) => grep_read(&w0),
        // `git grep -n` always prints `path:line:text`, relative to the folder it runs in.
        "git" if w0.get(1).map(String::as_str) == Some("grep") && stages[1..].iter().all(|s| cuts_only(s)) => grep_read(&w0[1..]).map(|_| ShellRead::Grep { single: None }),
        _ => None,
    }
}

/// A `grep -n` or `rg -n` stage: `Grep`, naming the one file searched when
/// its output carries no path.
fn grep_read(words: &[String]) -> Option<ShellRead> {
    let rg = words[0] == "rg";
    // Options that take the next word as their value.
    let valued: &[&str] = if rg { &["-e", "-f", "-A", "-B", "-C", "-m", "-g", "-t", "-T", "-j", "-M", "--glob", "--type", "--max-count", "--context", "--regexp"] } else { &["-e", "-f", "-A", "-B", "-C", "-m", "--regexp", "--file", "--context", "--max-count"] };
    let mut numbered = false;
    let mut with_name = false;
    let mut pattern_given = false;
    let mut pos: Vec<&String> = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let w = &words[i];
        if w == "--" {
            pos.extend(words[i + 1..].iter());
            break;
        }
        if valued.contains(&w.as_str()) {
            pattern_given |= w == "-e" || w == "-f" || w == "--regexp" || w == "--file";
            i += 2;
            continue;
        }
        if let Some(long) = w.strip_prefix("--") {
            numbered |= long == "line-number";
            with_name |= long == "with-filename";
            if long == "no-line-number" || long == "no-filename" || long == "count" || long == "files-with-matches" || long == "only-matching" || long == "json" || long == "heading" {
                return None;
            }
        } else if let Some(short) = w.strip_prefix('-').filter(|s| !s.is_empty()) {
            // Clustered short flags; a value-taking letter ends the cluster.
            for c in short.chars() {
                match c {
                    'n' => numbered = true,
                    'H' => with_name = true,
                    'h' | 'c' | 'l' | 'L' | 'o' | 'N' | 'I' => return None,
                    'e' | 'f' => {
                        pattern_given = true;
                        break;
                    }
                    'A' | 'B' | 'C' | 'm' => break,
                    _ => {}
                }
            }
        } else {
            pos.push(w);
        }
        i += 1;
    }
    if !numbered {
        return None;
    }
    let paths: Vec<&String> = if pattern_given { pos } else { pos.into_iter().skip(1).collect() };
    let single = (paths.len() == 1 && !with_name && !paths[0].contains(['*', '?'])).then(|| paths[0].clone());
    Some(ShellRead::Grep { single })
}

/// PowerShell: `Select-String` (its output is `path:line:text`) and
/// `Get-Content` windows (`-TotalCount N`, `| Select-Object -Skip S -First
/// N`, `(Get-Content F)[A..B]`).
fn ps_read(stages: &[&str], w0: &[String]) -> Option<ShellRead> {
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let first = w0[0].as_str();
    // (Get-Content F)[A..B]
    if stages.len() == 1 && (first.eq_ignore_ascii_case("(Get-Content") || first.eq_ignore_ascii_case("(gc")) {
        let s = stages[0];
        let close = s.rfind(")[")?;
        let inner = shell_words(&s[1..close])?;
        let idx = s[close + 2..].strip_suffix(']')?;
        let (a, b) = idx.split_once("..")?;
        let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
        let file = gc_file(&inner)?;
        return (b >= a).then(|| ShellRead::Window { file, from: a + 1, to: Some(b + 1), tail: None });
    }
    if eq(first, "Select-String") || eq(first, "sls") {
        // Select-String prints `path:line:text` for files (a relative path
        // inside the current folder), whatever their number.
        return stages[1..].iter().all(|s| cuts_only(s)).then_some(ShellRead::Grep { single: None });
    }
    if eq(first, "Get-ChildItem") || eq(first, "gci") || eq(first, "dir") || eq(first, "ls") {
        let s1 = shell_words(stages.get(1)?)?;
        return (s1.first().is_some_and(|w| eq(w, "Select-String") || eq(w, "sls")) && stages[2..].iter().all(|s| cuts_only(s))).then_some(ShellRead::Grep { single: None });
    }
    if !(eq(first, "Get-Content") || eq(first, "gc") || eq(first, "cat") || eq(first, "type")) {
        return None;
    }
    let file = gc_file(w0)?;
    let opt = |words: &[String], name: &str| -> Option<u32> {
        let i = words.iter().position(|w| eq(w, name))?;
        words.get(i + 1)?.parse().ok()
    };
    match stages.len() {
        1 => {
            if let Some(n) = opt(w0, "-TotalCount").or_else(|| opt(w0, "-Head")).or_else(|| opt(w0, "-First")) {
                return Some(ShellRead::Window { file, from: 1, to: Some(n), tail: None });
            }
            let n = opt(w0, "-Tail")?;
            Some(ShellRead::Window { file, from: 1, to: None, tail: Some(n) })
        }
        2 => {
            let s1 = shell_words(stages[1])?;
            if !s1.first().is_some_and(|w| eq(w, "Select-Object") || eq(w, "select")) {
                return None;
            }
            let skip = opt(&s1, "-Skip").unwrap_or(0);
            match (opt(&s1, "-First"), opt(&s1, "-Last")) {
                (Some(n), None) if n > 0 => Some(ShellRead::Window { file, from: skip + 1, to: Some(skip + n), tail: None }),
                (None, Some(n)) if skip == 0 && n > 0 => Some(ShellRead::Window { file, from: 1, to: None, tail: Some(n) }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The file a `Get-Content` names: `-Path F`, `-LiteralPath F` or the
/// first positional word.
fn gc_file(words: &[String]) -> Option<String> {
    let mut i = 1;
    let mut first_pos = None;
    while i < words.len() {
        let w = &words[i];
        if w.eq_ignore_ascii_case("-Path") || w.eq_ignore_ascii_case("-LiteralPath") {
            return words.get(i + 1).cloned();
        }
        if w.starts_with('-') {
            // Every other Get-Content option here takes a value.
            i += 2;
            continue;
        }
        if first_pos.is_none() {
            first_pos = Some(w.clone());
        }
        i += 1;
    }
    first_pos.filter(|f| !f.contains(['*', '?']))
}

/// A path as written, made absolute against `dir`. On Windows a Git Bash
/// path (`/c/Users/...`) is the drive path (`C:/Users/...`).
fn absolute(file: &str, dir: &str) -> String {
    let file = msys(file);
    if Path::new(&file).is_absolute() || file.starts_with(['/', '\\']) || dir.is_empty() {
        file
    } else {
        format!("{}/{}", dir.trim_end_matches(['/', '\\']), file.trim_start_matches("./"))
    }
}

/// `/c/x` as `C:/x` on Windows; anything else as it is.
fn msys(p: &str) -> String {
    let b = p.as_bytes();
    if cfg!(windows) && b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/' {
        return format!("{}:{}", (b[1] as char).to_ascii_uppercase(), &p[2..]);
    }
    p.to_string()
}

/// One read of a shell command and the `stdout` lines (`from..to`) it
/// printed, with the folder its relative paths are read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// What the read is.
    pub read: ShellRead,
    /// Its folder.
    pub dir: String,
    /// First line index into `stdout.split('\n')`.
    pub from: usize,
    /// One past its last line.
    pub to: usize,
}

/// The parts of a sequence (`;` or `&&` outside quotes), or `None` when
/// it holds `||`, a lone `&`, a newline or nothing.
fn sequence(cmd: &str) -> Option<Vec<&str>> {
    let b = cmd.as_bytes();
    let mut q: Option<u8> = None;
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match q {
            Some(x) if c == x => q = None,
            Some(_) => {}
            None => match c {
                b'\'' | b'"' => q = Some(c),
                b'\n' => return None,
                b';' => {
                    parts.push(cmd[start..i].trim());
                    start = i + 1;
                }
                b'&' if b.get(i + 1) == Some(&b'&') => {
                    parts.push(cmd[start..i].trim());
                    start = i + 2;
                    i += 1;
                }
                b'&' if i > 0 && b[i - 1] == b'>' => {}
                b'&' => return None,
                b'|' if b.get(i + 1) == Some(&b'|') => return None,
                _ => {}
            },
        }
        i += 1;
    }
    parts.push(cmd[start..].trim());
    let parts: Vec<&str> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then_some(parts)
}

/// Splits a shell command's output into its reads (see the module docs).
///
/// A Bash sequence of reads is read when every two reads are separated by
/// an `echo TEXT` whose exact line appears in the output, so each read's
/// lines are known (`sed -n 1,9p a.rs; echo ----; grep -n x b.rs`). A
/// `cd DIR` part moves the folder later parts read from. Anything else in
/// the sequence (an assignment, another command) makes the whole command
/// unread. PowerShell commands are read alone.
pub fn segments(shell: &str, cmd: &str, stdout: &str, cwd: &str) -> Option<Vec<Segment>> {
    let lines: Vec<&str> = stdout.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    if shell == "PowerShell" {
        return shell_read(shell, cmd).map(|read| vec![Segment { read, dir: cwd.to_string(), from: 0, to: lines.len() }]);
    }
    let cleaned = cmd.replace(" 2>/dev/null", "").replace(" 2>&1", "");
    let parts = sequence(&cleaned)?;
    let mut dir = cwd.to_string();
    let mut out = Vec::new();
    let mut pending: Option<(ShellRead, String)> = None;
    let mut pos = 0;
    for part in parts {
        let w = shell_words(part)?;
        match w.first().map(String::as_str) {
            Some("cd") if w.len() == 2 => {
                dir = absolute(&w[1], &dir);
            }
            Some("echo") => {
                // `echo -n`/`-e` change what is printed: not a separator.
                let text = w[1..].join(" ");
                if text.is_empty() || matches!(w.get(1).map(String::as_str), Some("-n" | "-e" | "-E" | "-ne" | "-en")) {
                    return None;
                }
                let k = (pos..lines.len()).find(|&k| lines[k] == text)?;
                if let Some((read, d)) = pending.take() {
                    out.push(Segment { read, dir: d, from: pos, to: k });
                }
                pos = k + 1;
            }
            _ => {
                if pending.is_some() {
                    // Two reads with nothing between them: whose line is whose?
                    return None;
                }
                pending = Some((shell_read(shell, part)?, dir.clone()));
            }
        }
    }
    if let Some((read, d)) = pending {
        out.push(Segment { read, dir: d, from: pos, to: lines.len() });
    }
    (!out.is_empty()).then_some(out)
}

/// The most cut items a Read note names.
const READ_NOTE_ITEMS: usize = 6;

/// The `additionalContext` line for a Read window `a..=b`: the items the
/// window cuts (they start before it or end after it), or nothing when it
/// cuts none. Past [`READ_NOTE_ITEMS`] the innermost (shortest) are kept.
pub fn read_note(shown: &str, a: u32, b: u32, items: &[crate::code::Item]) -> String {
    let mut cut: Vec<&crate::code::Item> = items.iter().filter(|i| i.end > i.line && i.line <= b && i.end >= a && (i.line < a || i.end > b)).collect();
    if cut.is_empty() {
        return String::new();
    }
    cut.sort_by(|x, y| (x.end - x.line).cmp(&(y.end - y.line)).then(x.line.cmp(&y.line)));
    let more = cut.len().saturating_sub(READ_NOTE_ITEMS);
    cut.truncate(READ_NOTE_ITEMS);
    cut.sort_by(|x, y| x.line.cmp(&y.line).then(y.end.cmp(&x.end)));
    let mut out = format!("atlas: {shown} lines {a}-{b} cut");
    for i in cut {
        out.push_str(&format!(" [{}]", i.label()));
    }
    if more > 0 {
        out.push_str(&format!(" +{more} more"));
    }
    out
}

/// Where a Grep content line's match is: its file as written and its line
/// number, or `None` for a context line (`path-12-text`), a separator
/// (`--`) or a line without a number.
///
/// `single` is the searched file when the search named one file (its lines
/// carry no path).
fn grep_hit<'a>(line: &'a str, single: Option<&'a str>) -> Option<(&'a str, u32)> {
    // `Select-String -Context` marks a match line with `> `.
    let line = line.strip_prefix("> ").unwrap_or(line);
    let b = line.as_bytes();
    let digits = |from: usize| -> Option<(u32, usize)> {
        let n = b[from..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 || n > 9 {
            return None;
        }
        Some((line[from..from + n].parse().ok()?, from + n))
    };
    if let Some(f) = single {
        let (n, at) = digits(0)?;
        return (b.get(at) == Some(&b':')).then_some((f, n));
    }
    // The first `:N:` names the match; a drive letter's colon is followed
    // by a separator, not a digit.
    for k in 1..b.len() {
        if b[k] == b':' {
            if let Some((n, at)) = digits(k + 1) {
                if b.get(at) == Some(&b':') {
                    return Some((&line[..k], n));
                }
            }
        }
    }
    None
}

/// Grep content with each match line's enclosing item appended,
/// ` [fn name 10-20]`. Paths resolve against `cwd`; `items` answers a
/// file's items, `None` for a file the code index does not cover.
///
/// # Errors
///
/// Whatever `items` returns.
pub fn annotate_grep(content: &str, cwd: &str, single: Option<&str>, items: &mut dyn FnMut(&str) -> Result<Option<Vec<crate::code::Item>>>) -> Result<String> {
    let mut cache: std::collections::HashMap<String, Option<Vec<crate::code::Item>>> = std::collections::HashMap::new();
    let mut out = String::with_capacity(content.len() + content.len() / 4);
    for (i, line) in content.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let (body, cr) = match line.strip_suffix('\r') {
            Some(b) => (b, "\r"),
            None => (line, ""),
        };
        out.push_str(body);
        if let Some((file, n)) = grep_hit(body, single) {
            let abs = if Path::new(file).is_absolute() || cwd.is_empty() { file.to_string() } else { format!("{}/{}", cwd.trim_end_matches(['/', '\\']), file) };
            if !cache.contains_key(&abs) {
                let v = items(&abs)?;
                cache.insert(abs.clone(), v);
            }
            if let Some(it) = cache.get(&abs).and_then(|v| v.as_ref()).and_then(|v| crate::code::enclosing(v, n)) {
                out.push_str(" [");
                out.push_str(&it.label());
                out.push(']');
            }
        }
        out.push_str(cr);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::Item;

    fn it(name: &str, kind: &str, line: u32, end: u32) -> Item {
        Item { name: name.into(), kind: kind.into(), line, end }
    }

    #[test]
    fn relative_paths_never_split_a_character() {
        // A cwd whose length ends inside a multi-byte character of the path
        // (a combining accent here) used to panic in `split_at`.
        assert_eq!(relative("e\u{301}.rs", "S"), "e\u{301}.rs");
        assert_eq!(relative("/w/\u{e9}t\u{e9}/a.rs", "/w/e"), "/w/\u{e9}t\u{e9}/a.rs");
        assert_eq!(relative("/w/x/a.rs", "/w/x"), "a.rs");
    }

    #[test]
    fn grep_hits_parse_paths_drive_letters_and_context() {
        assert_eq!(grep_hit("src\\a.rs:12:    x", None), Some(("src\\a.rs", 12)));
        assert_eq!(grep_hit("C:\\p\\a.rs:7:fn f() {", None), Some(("C:\\p\\a.rs", 7)));
        assert_eq!(grep_hit("src/a.rs-12-    x", None), None);
        assert_eq!(grep_hit("--", None), None);
        assert_eq!(grep_hit("src/a.rs:text without a number", None), None);
        assert_eq!(grep_hit("2249:#[cfg(test)]", Some("f.rs")), Some(("f.rs", 2249)));
        assert_eq!(grep_hit("2249-context", Some("f.rs")), None);
    }

    #[test]
    fn grep_lines_get_the_innermost_item_and_nothing_else_changes() {
        let items = vec![it("Store", "impl", 1, 100), it("Store::open", "fn", 10, 30), it("X", "const", 40, 40)];
        let content = "src/a.rs:12:    let x = 1;\nsrc/a.rs-13-    ctx\nsrc/a.rs:40:const X: u8 = 1;\r\nsrc/a.rs:200:tail\nother.rs:5:no index";
        let mut seen = Vec::new();
        let out = annotate_grep(content, "/w", None, &mut |p| {
            seen.push(p.to_string());
            Ok(p.ends_with("a.rs").then(|| items.clone()))
        })
        .unwrap();
        assert_eq!(
            out,
            "src/a.rs:12:    let x = 1; [fn Store::open 10-30]\nsrc/a.rs-13-    ctx\nsrc/a.rs:40:const X: u8 = 1; [impl Store 1-100]\r\nsrc/a.rs:200:tail\nother.rs:5:no index"
        );
        assert_eq!(out.lines().count(), content.lines().count());
        // Each file is looked up once.
        assert_eq!(seen, vec!["/w/src/a.rs".to_string(), "/w/other.rs".to_string()]);
    }

    #[test]
    fn shell_reads_are_parsed_from_the_command_as_written() {
        let w = |f: &str, a: u32, b: Option<u32>, t: Option<u32>| Some(ShellRead::Window { file: f.into(), from: a, to: b, tail: t });
        let g = |f: Option<&str>| Some(ShellRead::Grep { single: f.map(String::from) });
        let bash: &[(&str, Option<ShellRead>)] = &[
            ("sed -n '120,180p' src/store.rs", w("src/store.rs", 120, Some(180), None)),
            ("sed -n \"5p\" a.rs", w("a.rs", 5, Some(5), None)),
            ("cd sub && sed -n 10,20p a.rs", w("a.rs", 10, Some(20), None)),
            ("cat -n src/a.rs | sed -n '30,60p'", w("src/a.rs", 30, Some(60), None)),
            ("head -n 40 src/a.rs", w("src/a.rs", 1, Some(40), None)),
            ("head -40 src/a.rs 2>/dev/null", w("src/a.rs", 1, Some(40), None)),
            ("tail -n 25 src/a.rs", w("src/a.rs", 1, None, Some(25))),
            ("grep -n 'fn open' src/a.rs", g(Some("src/a.rs"))),
            ("grep -rn report_miss src", g(Some("src"))),
            ("grep -n -e x -e y a.rs b.rs", g(None)),
            ("rg -n \"fn (open|close)\" src/ | head -20", g(Some("src/"))),
            ("rg -n -C 2 pat", g(None)),
            ("grep -nH pat a.rs", g(None)),
            ("grep -rn pat src/*.rs", g(None)),
            // Not readable: no numbers, counts, file lists, chains, redirects.
            ("grep pat a.rs", None),
            ("grep -c pat a.rs", None),
            ("rg -l pat", None),
            ("sed -n '1,9p;20q' a.rs", None),
            ("sed -i 's/a/b/' a.rs", None),
            ("sed -n '1,9p' a.rs > out.txt", None),
            ("grep -n x a.rs; cat b.rs", None),
            ("cat a.rs", None),
            ("head -c 100 a.rs", None),
        ];
        for (cmd, want) in bash {
            assert_eq!(shell_read("Bash", cmd), *want, "{cmd}");
        }
        let ps: &[(&str, Option<ShellRead>)] = &[
            ("Select-String -Path src\\a.rs -Pattern 'fn open'", g(None)),
            ("Get-ChildItem -Recurse src -Filter *.rs | Select-String 'report_miss' | Select-Object -First 20", g(None)),
            ("Get-Content src\\a.rs -TotalCount 50", w("src\\a.rs", 1, Some(50), None)),
            ("Get-Content -Path src\\a.rs | Select-Object -Skip 99 -First 40", w("src\\a.rs", 100, Some(139), None)),
            ("(Get-Content src\\a.rs)[99..139]", w("src\\a.rs", 100, Some(140), None)),
            ("Get-Content src\\a.rs -Tail 30", w("src\\a.rs", 1, None, Some(30))),
            ("Get-Content src\\a.rs", None),
            ("Get-Content src\\a.rs | Select-String x", None),
        ];
        for (cmd, want) in ps {
            assert_eq!(shell_read("PowerShell", cmd), *want, "{cmd}");
        }
        assert_eq!(shell_read("Bash", "git grep -n -E 'fn (a|b)' -- src"), g(None));
        assert_eq!(shell_read("Bash", "git grep pat"), None);
        assert_eq!(grep_hit("> src\\a.rs:12:    x", None), Some(("src\\a.rs", 12)));
    }

    #[test]
    fn sequences_split_on_their_echo_lines() {
        let w = |f: &str, a: u32, b: u32| ShellRead::Window { file: f.into(), from: a, to: Some(b), tail: None };
        let out = "l1\nl2\n----\nsrc/b.rs:7:x\n----MAIN\n9:y";
        let segs = segments("Bash", "cd /w/sub; sed -n 1,2p a.rs; echo ----; git grep -n x -- src; echo ----MAIN; grep -n y main.rs", out, "/w").unwrap();
        assert_eq!(
            segs,
            [
                Segment { read: w("a.rs", 1, 2), dir: msys("/w/sub"), from: 0, to: 2 },
                Segment { read: ShellRead::Grep { single: None }, dir: msys("/w/sub"), from: 3, to: 4 },
                Segment { read: ShellRead::Grep { single: Some("main.rs".into()) }, dir: msys("/w/sub"), from: 5, to: 6 },
            ]
        );
        // Two reads with no separator, an assignment, or a missing echo line: unread.
        assert!(segments("Bash", "sed -n 1,2p a.rs; sed -n 3,4p a.rs", out, "/w").is_none());
        assert!(segments("Bash", "P=src; sed -n 1,2p $P/a.rs", out, "/w").is_none());
        assert!(segments("Bash", "sed -n 1,2p a.rs; echo ====; sed -n 3,4p a.rs", out, "/w").is_none());
        assert!(segments("Bash", "sed -n 1,2p a.rs || true", out, "/w").is_none());
        if cfg!(windows) {
            assert_eq!(absolute("/c/p/a.rs", "D:/x"), "C:/p/a.rs");
        }
    }

    #[test]
    fn read_notes_name_only_the_items_a_window_cuts() {
        let items = vec![it("Store", "impl", 1, 300), it("a", "fn", 10, 30), it("b", "fn", 35, 60), it("c", "fn", 70, 90)];
        assert_eq!(read_note("x.rs", 20, 65, &items), "atlas: x.rs lines 20-65 cut [impl Store 1-300] [fn a 10-30]");
        assert_eq!(read_note("x.rs", 1, 300, &items), "");
        assert_eq!(read_note("x.rs", 36, 50, &items), "atlas: x.rs lines 36-50 cut [impl Store 1-300] [fn b 35-60]");
    }
}
