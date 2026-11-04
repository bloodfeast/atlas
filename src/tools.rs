//! The MCP tools: their schemas, plain descriptions and handlers.
//!
//! Every handler first brings the index up to date with a cheap rescan,
//! then works only through [`crate::paths`] (confinement) and
//! [`crate::store::Store`] (locking, rows and the audit log). No tool deletes
//! or renames anything.

use std::collections::HashMap;

use crate::regex::RegexBuilder;
use crate::json::{Map, Value};
use crate::json;

use crate::error::{Error, Result};
use crate::fsio::{self, MAX_WRITE};
use crate::index::{self, RowKind, Spec};
use crate::links;
use crate::markdown;
use crate::paths::{self, Kind, Vault};
use crate::store::{Actor, NoteRow, Store, WriteFilter, WriteResult};

#[path = "tools_threads.rs"]
mod threads;
use threads::is_thread_row;

#[path = "tools_atlas.rs"]
mod atlas_tools;
pub use atlas_tools::{plain_query, MAX_MENTIONS, MAX_SECTIONS};
use atlas_tools::ATLAS_TOOLS;

#[path = "tools_code.rs"]
mod code_tools;

#[path = "tools_memory.rs"]
mod memory_tools;

#[path = "tools_lean.rs"]
mod lean_tools;

#[path = "tools_intake.rs"]
mod intake_tools;

#[path = "tools_dedup.rs"]
pub mod dedup;

#[path = "tools_surface.rs"]
mod surface;
pub use surface::{help, ops, HELP_URI};
pub use memory_tools::MemState;
use memory_tools::MEMORY_TOOLS;

/// Hard cap on search results.
pub const MAX_RESULTS: usize = 200;
/// Hard cap on notes returned by a listing.
pub const MAX_LIST: usize = 2000;

/// One tool's static description.
struct ToolDef {
    name: &'static str,
    description: &'static str,
    read_only: bool,
    schema: fn() -> Value,
}

fn s(desc: &str) -> Value {
    if desc.is_empty() {
        json!({"type": "string"})
    } else {
        json!({"type": "string", "description": desc})
    }
}
fn b(desc: &str) -> Value {
    if desc.is_empty() {
        json!({"type": "boolean"})
    } else {
        json!({"type": "boolean", "description": desc})
    }
}
fn n(desc: &str) -> Value {
    if desc.is_empty() {
        json!({"type": "integer"})
    } else {
        json!({"type": "integer", "description": desc})
    }
}
fn strs(desc: &str) -> Value {
    json!({"type": "array", "items": {"type": "string"}, "description": desc})
}
fn e(values: &[&str]) -> Value {
    json!({"type": "string", "enum": values})
}
/// An object schema. Unknown arguments are refused by [`Server::call`]
/// itself, so the schema leaves `additionalProperties` out, and an empty
/// `required` is left out too: every byte here is sent each time the tools
/// are listed.
fn obj(props: Value, required: &[&str]) -> Value {
    if required.is_empty() {
        json!({"type": "object", "properties": props})
    } else {
        json!({"type": "object", "properties": props, "required": required})
    }
}
/// Adds the output options shared by the plain-text tools.
fn with_fmt(mut props: Value) -> Value {
    props["format"] = e(&["text", "json"]);
    props["verbose"] = b("Add scan times and timings.");
    props
}
fn refresh() -> Value {
    json!({"type": "string", "enum": ["auto", "check", "off"], "description": "Stale files: auto reindexes a few (default), check reports."})
}

const SHA: &str = "Refuse unless the note's sha256 matches.";

const TOOLS: &[ToolDef] = &[
    ToolDef { name: "list_vaults", description: "Served vaults and note counts.", read_only: true, schema: || obj(json!({}), &[]) },
    ToolDef {
        name: "list_notes",
        description: "Notes in a vault folder: path, title, size, sha256.",
        read_only: true,
        schema: || {
            obj(
                json!({"vault": s(""), "folder": s(""), "glob": s("* ? **; no '/' matches the file name."), "recursive": b("Default true."), "limit": n("Default 500.")}),
                &["vault"],
            )
        },
    },
    ToolDef {
        name: "read_note",
        description: "A note's text and sha256; lines or budget_tokens for part of it.",
        read_only: true,
        schema: || {
            obj(
                json!({"vault": s(""), "path": s(""), "lines": s("A-B, A- or A: only these lines (from 1)."), "budget_tokens": n("Whole lines up to about this many tokens; a +N more lines line names the rest.")}),
                &["vault", "path"],
            )
        },
    },
    ToolDef {
        name: "search",
        description: "Search lines (substring, regex, fts) in vaults, or rank sections of every vault and source (sections; semantic and hybrid by meaning).",
        read_only: true,
        schema: || {
            obj(
                with_fmt(json!({
                    "query": s("sections: words, \"quoted phrase\"."),
                    "mode": e(&["substring", "regex", "fts", "sections", "semantic", "hybrid"]),
                    "vault": s(""),
                    "source": s("sections: one source or vault."),
                    "folder": s(""),
                    "case_sensitive": b(""),
                    "raw": b("sections: query is FTS5 syntax."),
                    "full": b("sections: whole section text, not snippets."),
                    "refresh": refresh(),
                    "limit": n("Default 50; sections 5.")
                })),
                &["query"],
            )
        },
    },
    ToolDef {
        name: "lookup",
        description: "Where an identifier is defined and mentioned, across every vault and source.",
        read_only: true,
        schema: || {
            obj(
                with_fmt(json!({"id": s(""), "limit": n("Mention lines, default 3."), "full": b("Every definition's whole text."), "refresh": refresh()})),
                &["id"],
            )
        },
    },
    ToolDef {
        name: "symbol",
        description: "Code symbol (Rust, WGSL, Protobuf, Go, TS/JS, C#, Python, Java; Kotlin and other files definitions only): definitions, references with their caller, impls.",
        read_only: true,
        schema: || obj(with_fmt(json!({"name": s("Name or Type::name."), "limit": n("References, default 10.")})), &["name"]),
    },
    ToolDef {
        name: "ids",
        description: "Registered identifiers: kind, definitions, occurrences, newest date.",
        read_only: true,
        schema: || {
            obj(
                with_fmt(json!({"prefix": s(""), "pattern": s("Regex."), "kind": s(""), "source": s(""), "limit": n("Default 50."), "refresh": refresh()})),
                &[],
            )
        },
    },
    ToolDef { name: "list_sources", description: "Vaults and sources: root, files, sections.", read_only: true, schema: || obj(with_fmt(json!({})), &[]) },
    ToolDef {
        name: "report_miss",
        description: "Record something grep or reading found that atlas did not return.",
        read_only: false,
        schema: || {
            obj(
                json!({"tool": e(&["lookup", "ids", "search", "list_sources", "other"]), "query": s(""), "returned": s(""), "found": s("path:line or description.")}),
                &["tool", "query", "found"],
            )
        },
    },
    ToolDef {
        name: "misses",
        description: "Recorded misses, newest first.",
        read_only: true,
        schema: || obj(with_fmt(json!({"status": e(&["open", "resolved", "all"]), "limit": n("Default 20.")})), &[]),
    },
    ToolDef { name: "resolve_miss", description: "Mark a miss resolved.", read_only: false, schema: || obj(json!({"id": n("")}), &["id"]) },
    ToolDef {
        name: "find_by_frontmatter",
        description: "Notes whose frontmatter key equals a value (any list item; case-insensitive), or has the key.",
        read_only: true,
        schema: || {
            obj(
                json!({"key": s(""), "value": s(""), "prefix": b("Value starts with `value`."), "vault": s(""), "folder": s(""), "limit": n("Default 500.")}),
                &["key"],
            )
        },
    },
    ToolDef { name: "backlinks", description: "Links into a note from every vault.", read_only: true, schema: || obj(json!({"vault": s(""), "path": s("")}), &["vault", "path"]) },
    ToolDef {
        name: "outlinks",
        description: "Links in a note, resolved (null when missing).",
        read_only: true,
        schema: || obj(json!({"vault": s(""), "path": s("")}), &["vault", "path"]),
    },
    ToolDef { name: "broken_links", description: "Links whose target note does not exist.", read_only: true, schema: || obj(json!({"vault": s("")}), &[]) },
    ToolDef {
        name: "write_log",
        description: "The audit log of writes, newest first.",
        read_only: true,
        schema: || {
            obj(
                json!({"vault": s(""), "path": s(""), "client": s(""), "since": s("UTC, inclusive."), "until": s("UTC, exclusive."), "discussion": s(""), "limit": n("Default 100.")}),
                &[],
            )
        },
    },
    ToolDef {
        name: "make_link",
        description: "Link text to a note: [[wikilink]] in one vault, obsidian:// across vaults.",
        read_only: true,
        schema: || obj(json!({"from_vault": s(""), "to_vault": s(""), "path": s(""), "heading": s(""), "text": s("")}), &["from_vault", "to_vault", "path"]),
    },
    ToolDef {
        name: "link_notes",
        description: "Append a link to the target under a section of the source note, once.",
        read_only: false,
        schema: || {
            obj(
                json!({"from_vault": s(""), "from_path": s(""), "to_vault": s(""), "to_path": s(""), "heading": s(""), "section": s("Default Related."), "reverse": b("Link back too.")}),
                &["from_vault", "from_path", "to_vault", "to_path"],
            )
        },
    },
    ToolDef {
        name: "create_note",
        description: "Create a note; fails if it exists.",
        read_only: false,
        schema: || obj(json!({"vault": s(""), "path": s(""), "content": s(""), "create_folders": b("")}), &["vault", "path", "content"]),
    },
    ToolDef {
        name: "append_to_note",
        description: "Append text to a note.",
        read_only: false,
        schema: || obj(json!({"vault": s(""), "path": s(""), "text": s(""), "expected_sha256": s(SHA)}), &["vault", "path", "text"]),
    },
    ToolDef {
        name: "append_history",
        description: "Append '- YYYY-MM-DD: text' under '## History'.",
        read_only: false,
        schema: || obj(json!({"vault": s(""), "path": s(""), "text": s(""), "date": s("Default today.")}), &["vault", "path", "text"]),
    },
    ToolDef {
        name: "set_frontmatter",
        description: "Set frontmatter keys; the rest stays byte-identical.",
        read_only: false,
        schema: || {
            obj(json!({"vault": s(""), "path": s(""), "values": {"type": "object"}, "expected_sha256": s(SHA)}), &["vault", "path", "values"])
        },
    },
    ToolDef {
        name: "replace_section",
        description: "Replace the body under a heading, or the preamble (heading \"\").",
        read_only: false,
        schema: || {
            obj(
                json!({"vault": s(""), "path": s(""), "heading": s("Without # marks."), "preamble": b(""), "level": n(""), "content": s(""), "expected_sha256": s("")}),
                &["vault", "path", "content", "expected_sha256"],
            )
        },
    },
    ToolDef {
        name: "next_id",
        description: "Allocate the next id of a sequence (PI-244); with name and content, create '<folder>/<id> <name>.md' ({{id}} replaced).",
        read_only: false,
        schema: || {
            obj(
                json!({"vault": s(""), "folder": s(""), "prefix": s("e.g. PI-."), "digits": n("Default 3."), "name": s(""), "content": s("")}),
                &["vault", "folder", "prefix"],
            )
        },
    },
    ToolDef {
        name: "build_index",
        description: "Write a generated index of notes between markers in a note (see README for spec).",
        read_only: false,
        schema: || obj(json!({"vault": s(""), "path": s(""), "name": s(""), "spec": {"type": "object"}}), &["vault", "path", "name", "spec"]),
    },
    ToolDef {
        name: "refresh_index",
        description: "Regenerate the indices in one note.",
        read_only: false,
        schema: || obj(json!({"vault": s(""), "path": s(""), "name": s("")}), &["vault", "path"]),
    },
    ToolDef { name: "refresh_all_indices", description: "Regenerate every index.", read_only: false, schema: || obj(json!({}), &[]) },
    ToolDef {
        name: "open_discussion",
        description: "Open a thread Discussions/D-### <topic>.md with P-1.",
        read_only: false,
        schema: || {
            obj(
                json!({
                    "vault": s(""),
                    "topic": s(""),
                    "subjects": {"type": "array", "items": {"type": "object", "properties": {"vault": s(""), "path": s(""), "heading": s("")}, "required": ["vault", "path"]}},
                    "participants": {"type": "array", "items": {"type": "string"}},
                    "opening_post": {"type": "object", "properties": {"author": s(""), "kind": e(&["claim", "question"]), "body": s("")}, "required": ["author", "body"]}
                }),
                &["vault", "topic", "subjects", "participants", "opening_post"],
            )
        },
    },
    ToolDef {
        name: "post",
        description: "Append a post to a thread. Correct with a retract; kinds other than claim and question need re.",
        read_only: false,
        schema: || {
            obj(
                json!({
                    "thread": s(THREAD),
                    "author": s(""),
                    "kind": e(&["claim", "evidence", "challenge", "answer", "concede", "retract", "question", "proposal"]),
                    "body": s(""),
                    "re": n("P-number replied to.")
                }),
                &["thread", "author", "kind", "body"],
            )
        },
    },
    ToolDef {
        name: "read_discussion",
        description: "A thread's posts, integrity check and outcomes.",
        read_only: true,
        schema: || obj(json!({"thread": s(THREAD), "since_post": n("")}), &["thread"]),
    },
    ToolDef {
        name: "discussion_status",
        description: "Each claim, challenge and question's state in a thread.",
        read_only: true,
        schema: || obj(json!({"thread": s(THREAD)}), &["thread"]),
    },
    ToolDef {
        name: "awaiting",
        description: "Challenges and questions to an author still unanswered.",
        read_only: true,
        schema: || obj(json!({"author": s(""), "vault": s("")}), &["author"]),
    },
    ToolDef {
        name: "conclude_discussion",
        description: "Append a verdict and conclude; refused while contested unless force_reason.",
        read_only: false,
        schema: || obj(json!({"thread": s(THREAD), "verdict_body": s(""), "author": s(""), "force_reason": s("")}), &["thread", "verdict_body"]),
    },
    ToolDef {
        name: "mark_unresolved",
        description: "End a thread without agreement, for a human decision.",
        read_only: false,
        schema: || {
            obj(
                json!({
                    "thread": s(THREAD),
                    "summary": s(""),
                    "open_questions": {"type": "array", "items": {"type": "string"}},
                    "positions": {"type": "array", "items": {"type": "object", "properties": {"participant": s(""), "position": s(""), "post": n("")}, "required": ["participant", "position", "post"]}},
                    "settle_by": s(""),
                    "author": s("")
                }),
                &["thread", "summary", "open_questions", "positions", "settle_by"],
            )
        },
    },
    ToolDef {
        name: "record_decision",
        description: "Settle an unresolved thread with a decision post.",
        read_only: false,
        schema: || obj(json!({"thread": s(THREAD), "decided_by": s(""), "decision": s("")}), &["thread", "decided_by", "decision"]),
    },
    ToolDef {
        name: "list_discussions",
        description: "Threads with status, participants and counts.",
        read_only: true,
        schema: || {
            obj(
                json!({"vault": s(""), "status": e(&["open", "concluded", "parked", "unresolved"]), "subject": s(""), "needs_decision": b("")}),
                &[],
            )
        },
    },
    ToolDef {
        name: "decision_queue",
        description: "Unresolved threads, oldest first.",
        read_only: true,
        schema: || obj(json!({"repo": s("Only threads naming this repo of the active project or a repo it talks to or is used by.")}), &[]),
    },
    ToolDef {
        name: "remember",
        description: "Store a memory, rule or decision (new key: create; existing key: new version, needs expected_sha). Never deletes.",
        read_only: false,
        schema: || {
            obj(
                json!({"kind": e(&["memory", "rule", "decision"]), "key": s("Memory slug; rules and decisions get one."), "title": s("One line."), "summary": s("Memory: index description."), "body": s(""), "sub": s("Memory type (user, feedback, project, reference)."), "scope": s("user: the shared user layer, seen in every project (memories only)."), "refs": strs("kind:target, e.g. lane:x, commit:abc."), "expected_sha": s("Head sha (or its first 12+ hex digits), to revise."), "export": s("Export to place it in."), "after": s("Place right after this key."), "author": s("Who asked.")}),
                &["kind", "body"],
            )
        },
    },
    ToolDef {
        name: "recall",
        description: "Memory records by key (history: true for versions), by query (best whole, the rest as pointers), or a lane's journal (lane).",
        read_only: true,
        schema: || obj(json!({"key": s(""), "query": s(""), "lane": s(""), "kind": s(""), "scope": s(""), "history": b(""), "limit": n("Default 8."), "budget_tokens": n("")}), &[]),
    },
    ToolDef {
        name: "supersede",
        description: "Replace records with a new one; the old stay, marked superseded. The way to record a reversal.",
        read_only: false,
        schema: || {
            obj(
                json!({"old": strs("Keys superseded."), "kind": e(&["memory", "rule", "decision"]), "key": s(""), "title": s(""), "summary": s(""), "body": s(""), "sub": s(""), "scope": s(""), "refs": strs(""), "author": s("")}),
                &["old", "body"],
            )
        },
    },
    ToolDef {
        name: "journal",
        description: "Append a lane journal entry: brief, decision, measurement (with conditions), commit, question, answered, next, note.",
        read_only: false,
        schema: || {
            obj(
                json!({"lane": s(""), "entry": e(crate::memory::ENTRIES), "text": s("First line is the headline."), "meta": s("key: value lines (branch, worktree, conditions)."), "refs": strs("answers:<key> for an answered question.")}),
                &["lane", "entry", "text"],
            )
        },
    },
    ToolDef {
        name: "handoff",
        description: "Write a lane handoff (allowed past the context cap). A fresh agent starts from recall lane=<id>.",
        read_only: false,
        schema: || obj(json!({"lane": s(""), "title": s("One line; default the text's first line."), "text": s(""), "refs": strs("prior:<key>, commit:<hash>, rule:<key>.")}), &["lane", "text"]),
    },
    ToolDef {
        name: "journal_status",
        description: "Lanes: entries, last entry, last commit, open questions, handoffs.",
        read_only: true,
        schema: || obj(json!({"repo": s("Only lanes naming this repo of the active project or a repo it talks to or is used by.")}), &[]),
    },
    ToolDef {
        name: "memory_log",
        description: "Memory versions, newest first.",
        read_only: true,
        schema: || obj(json!({"key": s(""), "kind": s(""), "scope": s(""), "since": s("UTC time prefix."), "author": s(""), "limit": n("Default 20.")}), &[]),
    },
    ToolDef {
        name: "hook",
        description: "Hook entry point for Claude Code mcp_tool hooks; not for models.",
        read_only: false,
        schema: || {
            obj(
                json!({"event": s(""), "session_id": s(""), "agent_id": s(""), "agent_type": s(""), "transcript_path": s(""), "prompt": s(""), "tool_name": s(""), "tool_prompt": s(""), "trigger": s(""), "source": s(""), "cwd": s(""), "permission_mode": s(""), "tool_input": s("JSON text."), "tool_response": s("JSON text.")}),
                &["event"],
            )
        },
    },
];

const THREAD: &str = "vault:D-004 or vault:path.";

/// The shortest reason a write tool accepts, in characters after trimming.
pub const MIN_REASON: usize = 12;
/// The longest reason a write tool accepts, in bytes.
pub const MAX_REASON: usize = 500;

/// Stock phrases that say nothing about why a change was made. Compared
/// after lowercasing, dropping punctuation and collapsing spaces.
const PLACEHOLDERS: &[&str] = &[
    "update",
    "updates",
    "updated",
    "fix",
    "fixes",
    "fixed",
    "n a",
    "na",
    "none",
    "edit",
    "edits",
    "edited",
    "change",
    "changes",
    "changed",
    "wip",
    "tbd",
    "todo",
    "misc",
    "test",
    "testing",
    "placeholder",
    "reason",
    "no reason",
    "no reason given",
    "not applicable",
    "minor update",
    "minor updates",
    "minor change",
    "minor changes",
    "minor edit",
    "minor edits",
    "small update",
    "small change",
    "small fix",
    "various changes",
    "various updates",
    "general update",
    "general updates",
    "update note",
    "update the note",
    "updating note",
    "edit note",
    "edit the note",
    "fix note",
    "see above",
    "as requested",
    "per request",
    "as asked",
    "housekeeping",
    "cleanup",
    "clean up",
    "routine update",
    "automated update",
];

/// Checks the `reason` a write tool must carry.
///
/// # Arguments
///
/// * `reason` - the caller's reason, or `None` when it was not given.
///
/// # Returns
///
/// The reason, trimmed.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if it is missing, not a string, blank, shorter
/// than [`MIN_REASON`] characters after trimming, longer than [`MAX_REASON`]
/// bytes, spans several lines, or is a placeholder such as `update`, `fix`,
/// `n/a`, `none` or `edit`.
pub fn check_reason(reason: Option<&Value>) -> Result<String> {
    let r = match reason {
        Some(Value::String(s)) => s.trim(),
        Some(Value::Null) | None => {
            return Err(Error::InvalidArgument("reason is required: say why this change is being made".into()));
        }
        Some(_) => return Err(Error::InvalidArgument("reason must be a string".into())),
    };
    if r.chars().count() < MIN_REASON {
        return Err(Error::InvalidArgument(format!(
            "reason must be at least {MIN_REASON} characters saying why this change is being made"
        )));
    }
    if r.len() > MAX_REASON || r.contains(|c: char| c.is_control()) {
        return Err(Error::InvalidArgument(format!("reason must be one line of at most {MAX_REASON} bytes")));
    }
    let norm: String = r
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if PLACEHOLDERS.contains(&norm.as_str()) {
        return Err(Error::InvalidArgument(format!("reason {r:?} is a placeholder; say why this change is being made")));
    }
    Ok(r.to_string())
}

const REASON_DESC: &str = "Why (12+ characters, not a placeholder); logged.";

const DISCUSSION_DESC: &str = "Thread this comes from (vault:D-004).";

/// Write tools whose post body serves as the audit reason, so they take no
/// `reason` argument.
const SELF_REASONED: &[&str] = &["open_discussion", "post", "journal", "handoff", "hook"];
/// Write tools that act on a thread, so the thread is their `discussion`.
const THREAD_WRITES: &[&str] = &["open_discussion", "post", "conclude_discussion", "mark_unresolved", "record_decision", "remember", "supersede", "journal", "handoff"];

/// A tool's input schema; write tools gain the required `reason` and the
/// optional `discussion`.
fn input_schema(t: &ToolDef) -> Value {
    let mut schema = (t.schema)();
    if !t.read_only {
        if !SELF_REASONED.contains(&t.name) {
            schema["properties"]["reason"] = s(REASON_DESC);
            match schema["required"].as_array_mut() {
                Some(req) => req.push(json!("reason")),
                None => schema["required"] = json!(["reason"]),
            }
        }
        if !THREAD_WRITES.contains(&t.name) {
            schema["properties"]["discussion"] = s(DISCUSSION_DESC);
        }
    }
    schema
}

/// The core read verbs, marked always-loaded (`_meta["anthropic/alwaysLoad"]`)
/// so Claude Code puts their schemas in context at startup instead of
/// deferring them behind a tool search. Every other tool stays deferred.
pub const ALWAYS_LOADED: &[&str] = &["lookup", "search", "recall", "section", "grep", "symbol"];

/// One tool as `tools/list` writes it. A read-only tool says so; a write
/// tool says it destroys nothing (the MCP default for a non-read-only tool
/// is destructive).
fn listed(t: &ToolDef, schema: Value) -> Value {
    let annotations = if t.read_only { json!({"readOnlyHint": true}) } else { json!({"destructiveHint": false}) };
    let mut v = json!({"name": t.name, "description": t.description, "inputSchema": schema, "annotations": annotations});
    if ALWAYS_LOADED.contains(&t.name) {
        v["_meta"] = json!({"anthropic/alwaysLoad": true});
    }
    v
}

/// The tool list for `tools/list`, kept short: every byte of it is sent
/// each time a client loads the tools. A few tools with an `op` argument
/// (see [`surface`]); the full tools behind them stay callable by name.
pub fn tool_list() -> Vec<Value> {
    surface::LISTED.iter().map(|t| listed(t, (t.schema)())).collect()
}

/// Every full tool with its whole schema, every argument described: what
/// the listed tools run, and the names kept callable as aliases. The help
/// text ([`help`]) is written from it.
pub fn full_tool_list() -> Vec<Value> {
    all_tools().map(|t| listed(t, input_schema(t))).collect()
}

/// Every tool: the core list, then the handle tools and the intake tools.
fn all_tools() -> impl Iterator<Item = &'static ToolDef> {
    TOOLS.iter().chain(lean_tools::TOOLS).chain(intake_tools::TOOLS)
}

/// Whether a tool is served by the handle or intake modules (they never
/// rescan the vaults).
fn is_package_tool(name: &str) -> bool {
    lean_tools::TOOLS.iter().chain(intake_tools::TOOLS).any(|t| t.name == name)
}

fn arg_str<'a>(a: &'a Map, k: &str) -> Result<&'a str> {
    match a.get(k) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(Error::InvalidArgument(format!("{k} must be a string"))),
        None => Err(Error::InvalidArgument(format!("{k} is required"))),
    }
}
/// [`arg_str`] that refuses an empty or blank value.
fn arg_nonblank<'a>(a: &'a Map, k: &str) -> Result<&'a str> {
    let s = arg_str(a, k)?;
    if s.trim().is_empty() {
        return Err(Error::InvalidArgument(format!("{k} is empty")));
    }
    Ok(s)
}
fn opt_str<'a>(a: &'a Map, k: &str) -> Result<Option<&'a str>> {
    match a.get(k) {
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(Error::InvalidArgument(format!("{k} must be a string"))),
    }
}
fn opt_bool(a: &Map, k: &str, default: bool) -> Result<bool> {
    match a.get(k) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::Null) | None => Ok(default),
        Some(_) => Err(Error::InvalidArgument(format!("{k} must be a boolean"))),
    }
}
fn opt_uint(a: &Map, k: &str, default: usize, max: usize) -> Result<usize> {
    match a.get(k) {
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) if v as usize <= max => Ok(v as usize),
            _ => Err(Error::InvalidArgument(format!("{k} must be an integer from 0 to {max}"))),
        },
        Some(Value::Null) | None => Ok(default),
        Some(_) => Err(Error::InvalidArgument(format!("{k} must be an integer"))),
    }
}
fn write_text<'a>(a: &'a Map, k: &str) -> Result<&'a str> {
    let t = arg_str(a, k)?;
    if t.len() > MAX_WRITE {
        return Err(Error::TooLarge(format!("{k} is {} bytes; the write cap is {MAX_WRITE}", t.len())));
    }
    if t.contains('\0') {
        return Err(Error::InvalidArgument(format!("{k} contains NUL")));
    }
    Ok(t)
}

/// The hits of a note search (vault, path, line, snippet), written as JSON
/// objects without building a [`Value`] per hit.
struct SearchRows<'a>(&'a [(String, String, usize, String)]);

impl json::ToJson for SearchRows<'_> {
    fn to_json(&self) -> Value {
        Value::Array(self.0.iter().map(|(v, p, l, sn)| json!({"vault": v, "path": p, "line": l, "snippet": sn})).collect())
    }
    fn write_json(&self, out: &mut String) {
        out.push('[');
        for (i, (v, p, l, sn)) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let mut w = json::ObjectWriter::new(out);
            w.field("vault", v).field("path", p).field("line", l).field("snippet", sn);
            w.end();
        }
        out.push(']');
    }
}

fn wr(r: &WriteResult) -> Value {
    crate::json::to_value(r)
}

/// A tool's answer: plain text (the default for the list-shaped atlas
/// tools, see [`crate::text`]), a JSON value, or JSON already written.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// Compact plain text.
    Text(String),
    /// A JSON value.
    Json(Value),
    /// Compact JSON text, written once (by a tool straight from its
    /// results, or by [`Server::answer`] from a [`Answer::Json`]).
    JsonText(String),
}

impl Answer {
    /// The answer as the text a client reads: the text itself, or the JSON
    /// written compactly.
    pub fn into_text(self) -> String {
        match self {
            Answer::Text(t) | Answer::JsonText(t) => t,
            Answer::Json(v) => v.to_string(),
        }
    }
}

/// How a text-capable tool writes its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fmt {
    /// JSON rather than plain text.
    pub(crate) json: bool,
    /// Include scan times and timings.
    pub(crate) verbose: bool,
}

/// What a plain grep-and-read would do to answer the same question (see
/// [`crate::tokens`]): `lookup`'s id, `search`'s query (quotes dropped),
/// `ids`' prefix and `section`'s id or heading are grepped as literals;
/// `grep` is its own pattern; `symbol` greps the code; `read_note` reads
/// the note; `recall` greps its query over the exports, or reads the record
/// or lane it names. Writes and tools with no grep equivalent have none.
pub(crate) fn probe_of(name: &str, args: &Value) -> crate::tokens::Probe {
    use crate::tokens::Probe;
    let get = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let phrase = |q: &str| q.replace('"', "").split_whitespace().collect::<Vec<_>>().join(" ");
    let lit = |t: String| Probe::Literal { text: t, source: None, path: None };
    let some = |o: Option<Probe>| o.unwrap_or_default();
    match name {
        "lookup" => some(get("id").map(|t| lit(t.to_string()))),
        "search" => some(get("query").map(|q| lit(phrase(q)))),
        "ids" => some(get("prefix").map(|t| lit(t.to_string()))),
        "symbol" => some(get("name").map(|n| Probe::Code(n.rsplit("::").next().unwrap_or(n).to_string()))),
        "section" => some(get("id").or_else(|| get("heading").map(|h| h.rsplit('>').next().unwrap_or(h).trim())).map(|t| Probe::Literal {
            text: t.to_string(),
            source: get("source").map(str::to_string),
            path: get("path").map(str::to_string),
        })),
        "grep" => some(get("pattern").map(|p| Probe::Regex { pattern: p.to_string(), source: get("source").map(str::to_string), path: get("path").map(str::to_string) })),
        "read_note" => match (get("vault"), get("path")) {
            (Some(v), Some(p)) => Probe::File { source: v.to_string(), path: p.to_string() },
            _ => Probe::None,
        },
        "recall" => match (get("key"), get("lane"), get("query")) {
            (Some(k), _, _) => Probe::Records(vec![k.to_string()]),
            (None, Some(l), _) => Probe::Records(vec![format!("lane:{l}")]),
            (None, None, Some(q)) => Probe::Exports(phrase(q)),
            _ => Probe::None,
        },
        _ => Probe::None,
    }
}

/// Server state shared by all tool calls of one process.
pub struct Server {
    store: Store,
    client: String,
    /// The checked reason of the write call in progress.
    reason: String,
    /// The discussion thread (`vault:path`) the write call in progress cites.
    discussion: String,
    /// Where answers are accounted for, when token logging is on.
    ledger: Option<crate::tokens::Ledger>,
    /// The code index, when code roots are configured.
    code: Option<CodeState>,
    /// Semantic search state, when set up.
    semantic: Option<SemState>,
    /// The memory store and hooks, when set up.
    memory: Option<MemState>,
    /// Repeat answers per agent (see [`dedup`]).
    dedup: dedup::Dedup,
    /// Whether a `symbol` answer this session has carried the code roots'
    /// coverage notes (definitions only, scan only); later ones leave them out.
    symbol_coverage_shown: bool,
}

/// Semantic search state: the model and vectors, loaded on first use.
#[derive(Debug)]
pub struct SemState {
    /// The packed model file.
    pub path: std::path::PathBuf,
    /// The database file (the background embedder opens its own
    /// connection).
    pub db: std::path::PathBuf,
    /// Encoder threads for background embedding.
    pub threads: usize,
    /// The model, once loaded.
    pub model: Option<std::sync::Arc<crate::embed::Model>>,
    /// The section vectors, once loaded.
    pub index: Option<crate::semantic::VecIndex>,
    /// The background embedder, once started.
    pub embedder: Option<crate::semantic::Embedder>,
}

impl SemState {
    /// Semantic search over a model file, nothing loaded yet.
    ///
    /// # Arguments
    ///
    /// * `path` - the packed model.
    /// * `db` - the database file.
    /// * `threads` - encoder threads (config `semantic.threads`).
    pub fn new(path: std::path::PathBuf, db: std::path::PathBuf, threads: usize) -> SemState {
        let threads = threads.max(1);
        SemState { path, db, threads, model: None, index: None, embedder: None }
    }
}

/// The code index a server answers `symbol` from.
#[derive(Debug)]
pub struct CodeState {
    /// The configured roots and settings.
    pub setup: crate::code::CodeSetup,
    /// The background indexer, when running.
    pub indexer: Option<crate::code_index::Indexer>,
}

impl Server {
    /// Wraps a store.
    pub fn new(store: Store) -> Server {
        Server { store, client: "unknown".to_string(), reason: String::new(), discussion: String::new(), ledger: None, code: None, semantic: None, memory: None, dedup: dedup::Dedup::default(), symbol_coverage_shown: false }
    }

    fn actor(&self) -> Actor {
        Actor { client: self.client.clone(), reason: self.reason.clone(), discussion: self.discussion.clone() }
    }

    /// Records the MCP client's name (from `initialize`) for the audit log.
    pub fn set_client(&mut self, name: &str) {
        let clean: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
        self.client = if clean.is_empty() { "unknown".to_string() } else { clean };
    }

    /// The store, for tests and the `--rescan` command.
    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// One line naming the served vaults, for the `initialize` result.
    pub fn instructions(&self) -> String {
        let names: Vec<String> = self.store.vaults().all().iter().map(|v| format!("{} ({})", v.name, v.folder)).collect();
        let sources: Vec<&str> = self.store.sources().iter().map(|s| s.name.as_str()).collect();
        let head = if sources.is_empty() {
            format!("Markdown vaults served: {}.", names.join(", "))
        } else {
            format!("Markdown vaults served: {}. Read-only sources: {}.", names.join(", "), sources.join(", "))
        };
        format!("{head} Every op and argument: resource {HELP_URI} or note op=help.")
    }

    fn vault(&self, name: &str) -> Result<Vault> {
        self.store.vaults().get(name).cloned()
    }

    /// Runs a tool and returns its answer as JSON. The text-capable tools
    /// answer in JSON here unless `format` is `text`, in which case the text
    /// comes back as `{"text": ...}`.
    ///
    /// # Arguments
    ///
    /// * `name` - the tool name.
    /// * `args` - its arguments object (`null` is taken as `{}`).
    ///
    /// # Returns
    ///
    /// The tool's result object.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown tool, [`Error::InvalidArgument`]
    /// for unknown or malformed arguments, and whatever the tool refuses.
    pub fn call(&mut self, name: &str, args: &Value) -> Result<Value> {
        let Some((name, args)) = surface::route(name, args)? else {
            return Ok(json!({ "text": help() }));
        };
        match self.run(&name, &args, true)? {
            Answer::Json(v) => Ok(v),
            // Read back for callers that want a value (tests); the client
            // path, `answer`, never parses.
            Answer::JsonText(t) => json::parse(&t).map_err(|e| Error::Io(format!("answer is not JSON: {e}"))),
            Answer::Text(t) => Ok(json!({ "text": t })),
        }
    }

    /// Runs a tool and returns its answer as a client gets it: plain text
    /// for `lookup`, `ids`, `search`, `misses` and `list_sources` unless
    /// `format` is `json`, JSON for the rest.
    ///
    /// # Arguments
    ///
    /// * `name` - the tool name.
    /// * `args` - its arguments object (`null` is taken as `{}`).
    ///
    /// # Errors
    ///
    /// As [`Server::call`].
    ///
    /// A JSON value is written once here, and that one text goes both to
    /// the ledger and to the client ([`Answer::JsonText`]).
    pub fn answer(&mut self, name: &str, args: &Value) -> Result<Answer> {
        self.answer_call(name, args, None)
    }

    /// The handle a thread that reads hook calls itself (the hook worker's
    /// reader) notes them through, so the calls they announce get their
    /// callers (see [`dedup`]).
    pub fn announcer(&self) -> dedup::Announcer {
        self.dedup.announcer()
    }

    /// The stream place of the message about to be answered, for a server
    /// whose hooks are noted ahead by the reading thread (see
    /// [`dedup::Announcer`]); None sees every hook noted.
    pub fn set_horizon(&mut self, horizon: Option<u64>) {
        self.dedup.set_horizon(horizon);
    }

    /// [`Server::answer`] for a call the client sent with a tool_use_id
    /// (`_meta`), which picks the agent that announced it.
    ///
    /// # Errors
    ///
    /// As [`Server::call`].
    pub fn answer_call(&mut self, name: &str, args: &Value, use_id: Option<&str>) -> Result<Answer> {
        // The hook names the agent behind the next call (see [`dedup`]).
        if name == "hook" {
            self.dedup.hook(args);
        }
        let caller = if name == "hook" { None } else { self.dedup.caller(name, args, use_id) };
        // `fresh` is taken by every tool: the whole answer, never a reference.
        let (fresh, args) = match args.as_object().filter(|m| m.contains_key("fresh")) {
            Some(m) => {
                let fresh = matches!(m.get("fresh"), Some(Value::Bool(true)));
                let mut m = m.clone();
                m.remove("fresh");
                (fresh, Value::Object(m))
            }
            None => (false, args.clone()),
        };
        // A listed tool's op runs its full tool, and the ledger counts it
        // under that name (its counterfactual is the full tool's).
        let Some((routed, routed_args)) = surface::route(name, &args)? else {
            return Ok(Answer::Text(help()));
        };
        let (name, args) = (routed.as_str(), &routed_args);
        let mut answer = match self.run(name, args, false)? {
            Answer::Json(v) => Answer::JsonText(v.to_string()),
            a => a,
        };
        if let Some(c) = caller {
            let read_only = all_tools().find(|t| t.name == name).is_some_and(|t| t.read_only);
            let reuse = !fresh && dedup::dedupable(name, read_only, args);
            answer = match answer {
                Answer::Text(t) => Answer::Text(self.dedup.answer(&c, name, args, t, reuse)),
                Answer::JsonText(t) => Answer::JsonText(self.dedup.answer(&c, name, args, t, reuse)),
                a => a,
            };
        }
        if let Some(l) = &self.ledger {
            let returned = match &answer {
                Answer::Text(t) | Answer::JsonText(t) => t.len(),
                Answer::Json(v) => v.to_string().len(),
            };
            l.record(crate::tokens::Record { tool: name.to_string(), returned, probe: probe_of(name, args) });
        }
        Ok(answer)
    }

    /// Serves the code index (`symbol`).
    ///
    /// # Arguments
    ///
    /// * `code` - the setup and, usually, its running indexer.
    pub fn set_code(&mut self, code: CodeState) {
        self.code = Some(code);
    }

    /// Serves semantic and hybrid search.
    pub fn set_semantic(&mut self, s: SemState) {
        self.semantic = Some(s);
    }

    /// The semantic search state, if set up.
    pub fn semantic(&self) -> Option<&SemState> {
        self.semantic.as_ref()
    }

    /// The code index state, if configured.
    pub fn code(&self) -> Option<&CodeState> {
        self.code.as_ref()
    }

    /// Turns on token accounting (see [`crate::tokens`]): every answer
    /// from [`Server::answer`] is logged with its counterfactual.
    pub fn set_ledger(&mut self, ledger: crate::tokens::Ledger) {
        self.ledger = Some(ledger);
    }

    fn run(&mut self, name: &str, args: &Value, json_default: bool) -> Result<Answer> {
        let def = all_tools().find(|t| t.name == name).ok_or_else(|| Error::NotFound(format!("tool {name:?}")))?;
        let empty = Map::new();
        let a = match args {
            Value::Object(m) => m,
            Value::Null => &empty,
            _ => return Err(Error::InvalidArgument("arguments must be an object".into())),
        };
        let schema = input_schema(def);
        let props = schema["properties"].as_object().cloned().unwrap_or_default();
        // `format` is taken by every tool: text by default, JSON on request.
        if let Some(k) = a.keys().find(|k| !props.contains_key(*k) && !matches!(k.as_str(), "format" | "fresh")) {
            return Err(Error::InvalidArgument(format!("unknown argument {k:?} for {name}")));
        }
        // A write without a usable reason is refused before anything is read or written.
        self.reason = if def.read_only || SELF_REASONED.contains(&name) { String::new() } else { check_reason(a.get("reason"))? };
        self.discussion = String::new();
        if !def.read_only {
            if let Some(d) = opt_str(a, "discussion")? {
                let (v, res, _) = self.resolve_thread(d)?;
                self.discussion = format!("{}:{}", v.name, res.rel);
            }
        }
        let sections = name == "search" && matches!(a.get("mode").and_then(Value::as_str), Some("sections" | "semantic" | "hybrid"));
        if MEMORY_TOOLS.contains(&name) {
            return self.run_memory(name, a);
        }
        if !ATLAS_TOOLS.contains(&name) && !sections && !is_package_tool(name) {
            self.store.rescan()?;
        }
        let fmt = Fmt {
            json: match opt_str(a, "format")? {
                None => json_default,
                Some("json") => true,
                Some("text") => false,
                Some(o) => return Err(Error::InvalidArgument(format!("format {o:?}; use text or json"))),
            },
            verbose: opt_bool(a, "verbose", false)?,
        };
        if let Some(r) = self.run_lean(name, a, fmt) {
            return r;
        }
        if let Some(r) = self.run_intake(name, a, fmt) {
            return r;
        }
        let j = |r: Result<Value>| r.map(Answer::Json);
        match name {
            "search" if sections => self.search_sections(a, fmt),
            "search" => self.search(a, fmt),
            "lookup" => self.lookup(a, fmt),
            "symbol" => self.symbol(a, fmt),
            "ids" => self.ids(a, fmt),
            "list_sources" => self.list_sources(fmt),
            "misses" => self.misses(a, fmt),
            "read_note" if !fmt.json => self.read_note_text(a),
            _ if fmt.json => j(self.run_json(name, a)),
            // The full tools without a text form of their own: their JSON
            // as lines and tables, every element kept.
            _ => self.run_json(name, a).map(|v| Answer::Text(crate::text::value(&v))),
        }
    }

    fn run_json(&mut self, name: &str, a: &Map) -> Result<Value> {
        match name {
            "report_miss" => self.report_miss(a),
            "resolve_miss" => self.resolve_miss(a),
            "list_vaults" => self.list_vaults(),
            "list_notes" => self.list_notes(a),
            "read_note" => self.read_note(a),
            "find_by_frontmatter" => self.find_by_frontmatter(a),
            "backlinks" | "outlinks" => self.links(a, name == "backlinks"),
            "broken_links" => self.broken_links(a),
            "write_log" => self.write_log(a),
            "make_link" => {
                let link = self.make_link(
                    arg_str(a, "from_vault")?,
                    arg_str(a, "to_vault")?,
                    arg_str(a, "path")?,
                    opt_str(a, "heading")?,
                    opt_str(a, "text")?,
                )?;
                Ok(json!({ "link": link }))
            }
            "link_notes" => self.link_notes(a),
            "create_note" => self.create_note(a),
            "append_to_note" => self.append_to_note(a),
            "append_history" => self.append_history(a),
            "set_frontmatter" => self.set_frontmatter(a),
            "replace_section" => self.replace_section(a),
            "next_id" => self.next_id(a),
            "build_index" => self.build_index(a),
            "refresh_index" => {
                let v = self.vault(arg_str(a, "vault")?)?;
                let r = self.refresh_index(&v, arg_str(a, "path")?, opt_str(a, "name")?, "refresh_index")?;
                Ok(r)
            }
            "refresh_all_indices" => self.refresh_all(),
            "open_discussion" => self.open_discussion(a),
            "post" => self.post(a),
            "read_discussion" => self.read_discussion(a),
            "discussion_status" => self.discussion_status(a),
            "awaiting" => self.awaiting(a),
            "conclude_discussion" => self.conclude_discussion(a),
            "mark_unresolved" => self.mark_unresolved(a),
            "record_decision" => self.record_decision(a),
            "list_discussions" => self.list_discussions(a),
            "decision_queue" => self.decision_queue(a),
            _ => Err(Error::NotFound(format!("tool {name:?}"))),
        }
    }

    fn list_vaults(&self) -> Result<Value> {
        let mut out = Vec::new();
        for v in self.store.vaults().all() {
            let count = self.store.notes(Some(&v.name), "", true, None)?.len();
            out.push(json!({"name": v.name, "folder": v.folder, "notes": count}));
        }
        Ok(json!({ "vaults": out }))
    }

    fn folder_arg(&self, v: &Vault, folder: Option<&str>) -> Result<String> {
        match folder {
            None | Some("") => Ok(String::new()),
            Some(f) => {
                paths::split_rel(f, Kind::Folder)?;
                Ok(paths::resolve_folder(v, f)?.rel)
            }
        }
    }

    fn list_notes(&self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let folder = self.folder_arg(&v, opt_str(a, "folder")?)?;
        let glob = opt_str(a, "glob")?;
        if let Some(g) = glob {
            index::check_glob(g)?;
        }
        let limit = opt_uint(a, "limit", 500, MAX_LIST)?;
        let rows = self.store.notes(Some(&v.name), &folder, opt_bool(a, "recursive", true)?, glob)?;
        let total = rows.len();
        let notes: Vec<Value> =
            rows.iter().take(limit).map(|r| json!({"path": r.path, "title": r.title, "size": r.size, "sha256": r.sha256})).collect();
        Ok(json!({"vault": v.name, "folder": folder, "total": total, "truncated": total > limit, "notes": notes}))
    }

    fn read_note(&self, a: &Map) -> Result<Value> {
        let n = self.note_window(a)?;
        let mut v = json!({"vault": n.vault, "path": n.path, "sha256": n.sha, "size": n.size, "frontmatter": markdown::parse_frontmatter(&n.whole), "content": n.text()});
        if n.cut() {
            v["lines"] = json!(format!("{}-{}", n.first, n.last));
            v["total_lines"] = json!(n.total);
        }
        Ok(v)
    }

    /// A note as text: one `vault:path sha256=... bytes=N` line, then the
    /// note exactly as written (its frontmatter once, where it is, rather
    /// than again parsed beside it as the JSON answer has it). With `lines`
    /// or `budget_tokens`, the header names the lines shown, and a cut by
    /// the budget ends with a `+N more lines (lines=A-B)` line.
    fn read_note_text(&self, a: &Map) -> Result<Answer> {
        let n = self.note_window(a)?;
        let mut out = format!("{}:{} sha256={} bytes={}", n.vault, n.path, n.sha, n.size);
        if n.cut() {
            out.push_str(&format!(" lines {}-{} of {}", n.first, n.last, n.total));
        }
        out.push('\n');
        out.push_str(&n.text());
        if n.last < n.until {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("+{} more lines (lines={}-{})\n", n.until - n.last, n.last + 1, n.until));
        }
        Ok(Answer::Text(out))
    }

    /// A vault note or a read-only source's file, read through the vault's
    /// or the source's own path checks (dot names, the deny-list on the
    /// path and on what it resolves to, a source's globs): the vault or
    /// source name, the path, the bytes.
    pub(crate) fn read_confined(&self, name: &str, rel: &str) -> Result<(String, String, Vec<u8>)> {
        Ok(match (self.vault(name), self.store.sources().iter().find(|s| s.name == name)) {
            (Err(_), Some(s)) => (s.name.clone(), rel.to_string(), s.read(rel)?.0),
            (v, _) => {
                let v = v?;
                let res = paths::resolve_existing(&v, rel)?;
                (v.name, res.rel, fsio::read_capped(&res.full)?)
            }
        })
    }

    /// The note `read_note` names and the lines it shows: `lines` (`A-B`,
    /// `A-` or `A`, 1-based and inclusive), then whole lines up to about
    /// `budget_tokens`, the first line always.
    fn note_window(&self, a: &Map) -> Result<NoteWindow> {
        let name = arg_str(a, "vault")?;
        let rel = arg_str(a, "path")?;
        // A read-only source's file reads as a note does (a recall pointer's
        // anchor names one), through the source's own path checks.
        let (vault, path, bytes) = self.read_confined(name, rel)?;
        let sha = crate::sha256::hex(&bytes);
        let size = bytes.len();
        let whole = String::from_utf8(bytes).map_err(|_| Error::InvalidArgument("note is not valid UTF-8".into()))?;
        let starts: Vec<usize> = std::iter::once(0).chain(whole.match_indices('\n').map(|(i, _)| i + 1)).filter(|&i| i < whole.len()).collect();
        let total = starts.len();
        let (from, until) = match opt_str(a, "lines")? {
            None => (1, total),
            Some(r) => {
                let bad = || Error::InvalidArgument(format!("lines {r:?}; use A-B, A- or A (from 1)"));
                let (x, y) = r.split_once('-').unwrap_or((r, r));
                let x: usize = x.trim().parse().map_err(|_| bad())?;
                let y: usize = if y.trim().is_empty() { total } else { y.trim().parse().map_err(|_| bad())? };
                if x == 0 || y < x {
                    return Err(bad());
                }
                (x, y.min(total))
            }
        };
        let budget = match a.get("budget_tokens") {
            None | Some(Value::Null) => usize::MAX,
            Some(_) => (opt_uint(a, "budget_tokens", 0, 1_000_000)? as f64 * crate::tokens::DEFAULT_BYTES_PER_TOKEN) as usize,
        };
        let end_of = |l: usize| if l < total { starts[l] } else { whole.len() };
        // The first line always; then each next line while the budget holds.
        let last = if from > until {
            from - 1
        } else {
            let mut last = from;
            while last < until && end_of(last + 1) - starts[from - 1] <= budget {
                last += 1;
            }
            last
        };
        Ok(NoteWindow { vault, path, sha, size, total, first: from, last, until, starts, whole })
    }

    fn scope(&self, a: &Map) -> Result<(Option<String>, String)> {
        let vault = opt_str(a, "vault")?;
        let folder = opt_str(a, "folder")?;
        match vault {
            Some(vn) => {
                let v = self.vault(vn)?;
                let f = self.folder_arg(&v, folder)?;
                Ok((Some(v.name), f))
            }
            None if folder.is_some_and(|f| !f.is_empty()) => Err(Error::InvalidArgument("folder needs vault".into())),
            None => Ok((None, String::new())),
        }
    }

    fn search(&self, a: &Map, f: Fmt) -> Result<Answer> {
        let query = arg_str(a, "query")?;
        if query.is_empty() || query.len() > 512 {
            return Err(Error::InvalidArgument("query must be 1 to 512 bytes".into()));
        }
        let mode = opt_str(a, "mode")?.unwrap_or("substring");
        if let Some(k) = ["source", "raw", "refresh", "full"].into_iter().find(|k| a.contains_key(*k)) {
            return Err(Error::InvalidArgument(format!("{k} applies to mode sections only")));
        }
        let limit = opt_uint(a, "limit", 50, MAX_RESULTS)?.max(1);
        let case = opt_bool(a, "case_sensitive", false)?;
        let (vault, folder) = self.scope(a)?;
        let prefix = if folder.is_empty() { String::new() } else { format!("{folder}/") };
        let mut results = Vec::new();
        let mut truncated = false;
        // Each hit: vault, path, line, and the byte range matched in the
        // line (the snippet is centred on it).
        let mut push = |v: &str, p: &str, line: usize, text: &str, at: (usize, usize), results: &mut Vec<(String, String, usize, String)>| -> bool {
            if results.len() >= limit {
                truncated = true;
                return false;
            }
            results.push((v.to_string(), p.to_string(), line, crate::text::window(text.trim_end(), at.0, at.1, crate::text::SNIPPET_CHARS)));
            true
        };
        match mode {
            "substring" | "regex" => {
                let re = if mode == "regex" {
                    RegexBuilder::new(query)
                        .case_insensitive(!case)
                        .build()
                        .map_err(|e| Error::InvalidArgument(format!("regex: {e}")))?
                } else {
                    RegexBuilder::new(&crate::regex::escape(query))
                        .case_insensitive(!case)
                        .build()
                        .map_err(|e| Error::InvalidArgument(format!("query: {e}")))?
                };
                'outer: for (v, p, body) in self.store.bodies(vault.as_deref())? {
                    if !p.starts_with(&prefix) {
                        continue;
                    }
                    for (i, line) in body.lines().enumerate() {
                        // A search over the backtracking budget would find
                        // nothing on this line; say so instead.
                        re.check_text(line.len()).map_err(|e| {
                            Error::InvalidArgument(format!("regex search refused at {v}:{p} line {}: {e}; or set folder to skip this note", i + 1))
                        })?;
                        if let Some(m) = re.find(line) {
                            if !push(&v, &p, i + 1, line, (m.start(), m.len()), &mut results) {
                                break 'outer;
                            }
                        }
                    }
                }
            }
            "fts" => {
                let terms: Vec<String> = query
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|t| t.len() > 1 && !matches!(*t, "AND" | "OR" | "NOT" | "NEAR"))
                    .map(str::to_lowercase)
                    .collect();
                'notes: for (v, p, body) in self.store.fts(query, vault.as_deref(), MAX_RESULTS * 4)? {
                    if !p.starts_with(&prefix) {
                        continue;
                    }
                    let mut hits = 0;
                    for (i, line) in body.lines().enumerate() {
                        let at = terms.iter().filter_map(|t| crate::text::find_ci(line, t).map(|i| (i, t.len()))).min();
                        if let Some(at) = at {
                            if !push(&v, &p, i + 1, line, at, &mut results) {
                                break 'notes;
                            }
                            hits += 1;
                            if hits == 5 {
                                break;
                            }
                        }
                    }
                    if hits == 0 && !push(&v, &p, 0, "", (0, 0), &mut results) {
                        break;
                    }
                }
            }
            other => return Err(Error::InvalidArgument(format!("mode {other:?}; use substring, regex, fts, sections, semantic or hybrid"))),
        }
        if f.json {
            let cap = 64 + results.iter().map(|r| r.3.len() + r.1.len() + 48).sum::<usize>();
            return Ok(Answer::JsonText(json::object(cap, |w| {
                w.field("mode", mode).field("truncated", &truncated).field("results", &SearchRows(&results));
            })));
        }
        let mut out = String::new();
        {
            let mut t = crate::text::Table::new(&mut out, &["loc", "snippet"]);
            for (v, p, l, sn) in &results {
                t.field(&format!("{v}:{p}:{l}")).field(sn);
            }
        }
        if truncated {
            out.push_str(&format!("+more (limit={limit})\n"));
        }
        // These modes read vault notes only: an empty answer says where
        // the read-only sources are searched.
        if results.is_empty() && !self.store.sources().is_empty() {
            out.push_str("no match in vault notes (this mode reads vaults only; sources: mode sections, or grep)\n");
        }
        Ok(Answer::Text(out))
    }

    fn find_by_frontmatter(&self, a: &Map) -> Result<Value> {
        let key = arg_str(a, "key")?;
        let value = opt_str(a, "value")?;
        let prefix = opt_bool(a, "prefix", false)?;
        let limit = opt_uint(a, "limit", 500, MAX_LIST)?;
        let (vault, folder) = self.scope(a)?;
        let rows = self.store.notes(vault.as_deref(), &folder, true, None)?;
        let filter = value.map(|v| index::Filter { key: key.to_string(), value: v.to_string(), prefix });
        let mut out = Vec::new();
        let mut total = 0;
        for r in &rows {
            let ok = match &filter {
                Some(f) => index::passes(r, f),
                None => r.frontmatter.contains_key(key),
            };
            if ok {
                total += 1;
                if out.len() < limit {
                    out.push(json!({"vault": r.vault, "path": r.path, "value": r.frontmatter.get(key).cloned().unwrap_or(Value::Null)}));
                }
            }
        }
        Ok(json!({"total": total, "truncated": total > limit, "notes": out}))
    }

    fn links(&self, a: &Map, back: bool) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let res = paths::resolve_existing(&v, arg_str(a, "path")?)?;
        let rows = if back { self.store.backlinks(&v.name, &res.rel)? } else { self.store.outlinks(&v.name, &res.rel)? };
        Ok(json!({"vault": v.name, "path": res.rel, "links": rows}))
    }

    fn write_log(&self, a: &Map) -> Result<Value> {
        let time = |k: &str| -> Result<Option<String>> {
            let Some(t) = opt_str(a, k)? else { return Ok(None) };
            let ok = t.len() >= 10
                && t.len() <= 24
                && t.bytes().enumerate().all(|(i, c)| match i {
                    4 | 7 => c == b'-',
                    10 => c == b'T',
                    13 | 16 => c == b':',
                    19 => c == b'.',
                    23 => c == b'Z',
                    _ => c.is_ascii_digit(),
                });
            if ok {
                Ok(Some(t.to_string()))
            } else {
                Err(Error::InvalidArgument(format!("{k} must look like 2026-10-08 or 2026-10-08T14:30:00 (UTC)")))
            }
        };
        let vault = match opt_str(a, "vault")? {
            Some(v) => Some(self.vault(v)?.name),
            None => None,
        };
        let path = opt_str(a, "path")?.map(str::to_string);
        if path.is_some() && vault.is_none() {
            return Err(Error::InvalidArgument("path needs vault".into()));
        }
        if let Some(p) = &path {
            paths::split_rel(p, Kind::Note)?;
        }
        let discussion = match opt_str(a, "discussion")? {
            Some(d) => {
                let (dv, dres, _) = self.resolve_thread(d)?;
                Some(format!("{}:{}", dv.name, dres.rel))
            }
            None => None,
        };
        let filter = WriteFilter {
            vault,
            path,
            client: opt_str(a, "client")?.map(str::to_string),
            since: time("since")?,
            until: time("until")?,
            discussion,
        };
        let limit = opt_uint(a, "limit", 100, MAX_LIST)?;
        let (entries, total) = self.store.write_log(&filter, limit)?;
        Ok(json!({"total": total, "truncated": total > entries.len(), "entries": entries}))
    }

    fn broken_links(&self, a: &Map) -> Result<Value> {
        let vault = match opt_str(a, "vault")? {
            Some(vn) => Some(self.vault(vn)?.name),
            None => None,
        };
        let rows = self.store.broken(vault.as_deref())?;
        let total = rows.len();
        let rows: Vec<_> = rows.into_iter().take(MAX_LIST).collect();
        Ok(json!({"total": total, "truncated": total > MAX_LIST, "links": rows}))
    }

    /// Builds a link from a note in `from_vault` to an existing note.
    fn make_link(&self, from_vault: &str, to_vault: &str, path: &str, heading: Option<&str>, text: Option<&str>) -> Result<String> {
        let from = self.vault(from_vault)?;
        let to = self.vault(to_vault)?;
        let res = paths::resolve_existing(&to, path)?;
        let heading = match heading {
            Some(h) if !h.is_empty() => {
                let body = fsio::read_text(&res.full)?;
                let found = markdown::headings(&body)
                    .into_iter()
                    .find(|x| x.text == h)
                    .or_else(|| markdown::headings(&body).into_iter().find(|x| x.text.eq_ignore_ascii_case(h)))
                    // `P-3` names a discussion post by number.
                    .or_else(|| {
                        let is_post = h.strip_prefix("P-").is_some_and(|d| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit()));
                        let prefix = format!("{h} ");
                        is_post
                            .then(|| markdown::headings(&body).into_iter().find(|x| x.level == 3 && x.text.starts_with(&prefix)))
                            .flatten()
                    })
                    .ok_or_else(|| Error::NotFound(format!("heading {h:?} in {}", res.rel)))?;
                Some(found.text)
            }
            _ => None,
        };
        if let Some(t) = text {
            if t.contains(['\n', '\r', ']']) || t.len() > 200 {
                return Err(Error::InvalidArgument("link text must be one line under 200 bytes without ']'".into()));
            }
        }
        if from.name == to.name {
            let target = if self.stem_unique(&to.name, &res.rel)? { links::stem(&res.rel).to_string() } else { res.rel.clone() };
            Ok(links::wikilink(&target, heading.as_deref(), text))
        } else {
            let t = text.map(str::to_string).unwrap_or_else(|| match &heading {
                Some(h) => format!("{} > {h}", links::stem(&res.rel)),
                None => links::stem(&res.rel).to_string(),
            });
            Ok(links::uri_link(&to.folder, &res.rel, heading.as_deref(), &t))
        }
    }

    fn link_notes(&mut self, a: &Map) -> Result<Value> {
        let fv = arg_str(a, "from_vault")?;
        let fp = arg_str(a, "from_path")?;
        let tv = arg_str(a, "to_vault")?;
        let tp = arg_str(a, "to_path")?;
        let heading = opt_str(a, "heading")?;
        let section = opt_str(a, "section")?.unwrap_or("Related");
        if section.is_empty() || section.len() > 120 || section.contains(['\n', '\r']) {
            return Err(Error::InvalidArgument("section must be one line, 1 to 120 bytes".into()));
        }
        let forward_link = self.make_link(fv, tv, tp, heading, None)?;
        let reverse = opt_bool(a, "reverse", false)?;
        let back_link = if reverse { Some(self.make_link(tv, fv, fp, None, None)?) } else { None };
        let who = self.actor();
        let fvault = self.vault(fv)?;
        let add = |link: String| {
            move |text: &str| -> Result<String> {
                if text.contains(&link) {
                    Ok(text.to_string())
                } else {
                    markdown::append_to_section(text, section, &format!("- {link}"))
                }
            }
        };
        let forward = self.store.modify("link_notes", &who, &fvault, fp, None, add(forward_link.clone()))?;
        let mut out = json!({"link": forward_link, "forward": wr(&forward)});
        if let Some(bl) = back_link {
            let tvault = self.vault(tv)?;
            let back = self.store.modify("link_notes", &who, &tvault, tp, None, add(bl.clone()))?;
            out["reverse_link"] = json!(bl);
            out["reverse"] = wr(&back);
        }
        Ok(out)
    }

    fn create_note(&mut self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let content = write_text(a, "content")?;
        let r = self.store.create("create_note", &self.actor(), &v, arg_str(a, "path")?, content, opt_bool(a, "create_folders", false)?)?;
        Ok(wr(&r))
    }

    fn append_to_note(&mut self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let add = write_text(a, "text")?.to_string();
        let who = self.actor();
        let r = self.store.modify("append_to_note", &who, &v, arg_str(a, "path")?, opt_str(a, "expected_sha256")?, |text| {
            let nl = markdown::eol(text);
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str(nl);
            }
            out.push_str(&markdown::to_eol(&add, nl));
            if !out.ends_with('\n') {
                out.push_str(nl);
            }
            Ok(out)
        })?;
        Ok(wr(&r))
    }

    fn append_history(&mut self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let entry = write_text(a, "text")?;
        if entry.trim().is_empty() || entry.contains(['\n', '\r']) {
            return Err(Error::InvalidArgument("text must be one non-empty line".into()));
        }
        let date = match opt_str(a, "date")? {
            Some(d) => {
                let ok = d.len() == 10 && d.bytes().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == b'-' } else { c.is_ascii_digit() });
                if !ok {
                    return Err(Error::InvalidArgument("date must be YYYY-MM-DD".into()));
                }
                d.to_string()
            }
            None => self.store.local_date()?,
        };
        let line = format!("- {date}: {}", entry.trim());
        let who = self.actor();
        let r = self
            .store
            .modify("append_history", &who, &v, arg_str(a, "path")?, None, |text| markdown::append_to_section(text, "History", &line))?;
        let mut out = wr(&r);
        out["line"] = json!(line);
        Ok(out)
    }

    fn set_frontmatter(&mut self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let values = match a.get("values") {
            Some(Value::Object(m)) => m.clone(),
            _ => return Err(Error::InvalidArgument("values must be an object".into())),
        };
        if values.len() > 64 || crate::json::to_string_ok(&values).map(|s| s.len()).unwrap_or(0) > MAX_WRITE {
            return Err(Error::TooLarge("values".into()));
        }
        let who = self.actor();
        let r = self.store.modify("set_frontmatter", &who, &v, arg_str(a, "path")?, opt_str(a, "expected_sha256")?, |text| {
            markdown::set_frontmatter(text, &values)
        })?;
        Ok(wr(&r))
    }

    fn replace_section(&mut self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let heading = opt_str(a, "heading")?.map(str::to_string);
        let preamble = opt_bool(a, "preamble", false)? || heading.as_deref() == Some("");
        if preamble && heading.as_deref().is_some_and(|h| !h.is_empty()) {
            return Err(Error::InvalidArgument("give a heading or preamble: true, not both".into()));
        }
        let heading = match (preamble, heading) {
            (true, _) => String::new(),
            (false, Some(h)) => h,
            (false, None) => return Err(Error::InvalidArgument("give heading, or preamble: true".into())),
        };
        let content = write_text(a, "content")?.to_string();
        let sha = arg_str(a, "expected_sha256")?;
        let level = match a.get("level") {
            Some(Value::Null) | None => None,
            _ => Some(opt_uint(a, "level", 0, 6)?).filter(|l| *l >= 1),
        };
        let who = self.actor();
        let r = self.store.modify("replace_section", &who, &v, arg_str(a, "path")?, Some(sha), |text| {
            if preamble {
                Ok(markdown::replace_preamble(text, &content))
            } else {
                markdown::replace_section(text, &heading, level, &content)
            }
        })?;
        Ok(wr(&r))
    }

    fn next_id(&mut self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let folder = arg_str(a, "folder")?;
        let prefix = arg_str(a, "prefix")?;
        if prefix.is_empty() || prefix.len() > 16 || !prefix.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            return Err(Error::InvalidArgument("prefix must be 1 to 16 characters of A-Z a-z 0-9 - _".into()));
        }
        let digits = opt_uint(a, "digits", 3, 9)?.max(1);
        let name = opt_str(a, "name")?;
        let content = match a.get("content") {
            Some(Value::String(_)) => Some(write_text(a, "content")?),
            _ => None,
        };
        let note = match (name, content) {
            (Some(n), Some(c)) => {
                if n.is_empty() || n.contains(['/', '\\']) {
                    return Err(Error::InvalidArgument("name must be a file name without folders".into()));
                }
                Some((n, c))
            }
            (None, None) => None,
            _ => return Err(Error::InvalidArgument("give both name and content, or neither".into())),
        };
        let who = self.actor();
        let (id, written) = self.store.next_id("next_id", &who, &v, folder, prefix, digits, note)?;
        Ok(json!({"id": id, "note": written.as_ref().map(wr)}))
    }

    fn rows_for(&self, spec: &Spec) -> Result<Vec<NoteRow>> {
        let kind = spec.rows.unwrap_or_default();
        if kind != RowKind::Notes && spec.sources.is_empty() {
            // Thread rows with no sources: every vault's Discussions folder.
            let mut rows = Vec::new();
            for v in self.store.vaults().all() {
                rows.extend(self.thread_rows(Some(&v.name))?);
            }
            return Ok(Self::keep_kind(rows, kind));
        }
        let mut rows = Vec::new();
        for src in &spec.sources {
            let v = self.vault(&src.vault)?;
            let folder = self.folder_arg(&v, Some(&src.folder))?;
            rows.extend(self.store.notes(Some(&v.name), &folder, true, src.glob.as_deref())?);
        }
        if kind == RowKind::Notes {
            return Ok(rows);
        }
        let rows = rows.into_iter().filter(is_thread_row).map(|r| self.augment_thread(r)).collect();
        Ok(Self::keep_kind(rows, kind))
    }

    fn keep_kind(rows: Vec<NoteRow>, kind: RowKind) -> Vec<NoteRow> {
        match kind {
            RowKind::DecisionQueue => {
                rows.into_iter().filter(|r| r.frontmatter.get("status").and_then(Value::as_str) == Some("unresolved")).collect()
            }
            _ => rows,
        }
    }

    fn render(&self, spec: &Spec, from: &Vault) -> Result<String> {
        spec.validate()?;
        let rows = self.rows_for(spec)?;
        let mut spec = spec.clone();
        if spec.rows == Some(RowKind::DecisionQueue) && spec.sort.is_none() {
            spec.sort = Some("unresolved_at".into());
        }
        let spec = &spec;
        let vaults = self.store.vaults().clone();
        let folder_of = move |name: &str| vaults.get(name).map(|v| v.folder.clone()).unwrap_or_else(|_| name.to_string());
        let counts = self.stem_counts()?;
        let unique = |n: &NoteRow| counts.get(&(n.vault.clone(), n.title.to_lowercase())).copied().unwrap_or(0) <= 1;
        Ok(index::render(spec, &from.name, &rows, &folder_of, &unique))
    }

    /// How many notes share each file name, per vault (case-insensitive).
    fn stem_counts(&self) -> Result<HashMap<(String, String), usize>> {
        let mut counts = HashMap::new();
        for n in self.store.notes(None, "", true, None)? {
            *counts.entry((n.vault, n.title.to_lowercase())).or_insert(0) += 1;
        }
        Ok(counts)
    }

    /// True when no other note in the vault has this note's file name, so a
    /// wikilink to it can drop the folder, as Obsidian's shortest-path links do.
    fn stem_unique(&self, vault: &str, path: &str) -> Result<bool> {
        let stem = links::stem(path).to_lowercase();
        Ok(self.store.notes(Some(vault), "", true, None)?.iter().filter(|n| n.title.to_lowercase() == stem).count() <= 1)
    }

    fn build_index(&mut self, a: &Map) -> Result<Value> {
        let v = self.vault(arg_str(a, "vault")?)?;
        let path = arg_str(a, "path")?;
        let name = arg_str(a, "name")?;
        index::check_name(name)?;
        let spec: Spec = crate::json::from_value(a.get("spec").cloned().unwrap_or(Value::Null))
            .map_err(|e| Error::InvalidArgument(format!("spec: {e}")))?;
        let body = self.render(&spec, &v)?;
        if body.len() > MAX_WRITE {
            return Err(Error::TooLarge(format!("the index renders to {} bytes; the write cap is {MAX_WRITE}", body.len())));
        }
        let who = self.actor();
        let r = match paths::resolve_existing(&v, path) {
            Ok(_) => self.store.modify("build_index", &who, &v, path, None, |text| index::splice(text, name, &spec, &body))?,
            Err(Error::NotFound(_)) => {
                let text = index::splice("", name, &spec, &body)?;
                self.store.create("build_index", &who, &v, path, &text, false)?
            }
            Err(e) => return Err(e),
        };
        self.store.record_index(&v.name, &r.path, name, &spec.to_marker_json(), &crate::sha256::hex(body.as_bytes()))?;
        Ok(wr(&r))
    }

    fn refresh_index(&mut self, v: &Vault, path: &str, only: Option<&str>, tool: &str) -> Result<Value> {
        let res = paths::resolve_existing(v, path)?;
        let text = fsio::read_text(&res.full)?;
        let regions = index::regions(&text)?;
        let mut rendered: HashMap<String, (Spec, String)> = HashMap::new();
        for r in &regions {
            if only.is_some_and(|o| o != r.name) {
                continue;
            }
            rendered.insert(r.name.clone(), (r.spec.clone(), self.render(&r.spec, v)?));
        }
        if let Some(o) = only {
            if !rendered.contains_key(o) {
                return Err(Error::NotFound(format!("index {o:?} in {}", res.rel)));
            }
        }
        let who = self.actor();
        let names: Vec<String> = {
            let mut n: Vec<String> = rendered.keys().cloned().collect();
            n.sort();
            n
        };
        let rendered_ref = &rendered;
        let r = self.store.modify(tool, &who, v, path, None, |current| {
            let mut out = current.to_string();
            for name in &names {
                let (spec, body) = &rendered_ref[name];
                let now = index::regions(&out)?.into_iter().find(|r| &r.name == name);
                match now {
                    Some(reg) if reg.spec == *spec => out = index::splice(&out, name, spec, body)?,
                    _ => return Err(Error::InvalidArgument(format!("index {name} changed while refreshing; retry"))),
                }
            }
            Ok(out)
        })?;
        for name in &names {
            let (spec, body) = &rendered[name];
            self.store.record_index(&v.name, &r.path, name, &spec.to_marker_json(), &crate::sha256::hex(body.as_bytes()))?;
        }
        let mut out = wr(&r);
        out["indices"] = json!(names);
        Ok(out)
    }

    fn refresh_all(&mut self) -> Result<Value> {
        let mut results = Vec::new();
        for (vault, path) in self.store.notes_with_indices()? {
            let v = self.vault(&vault)?;
            match self.refresh_index(&v, &path, None, "refresh_all_indices") {
                Ok(r) => results.push(r),
                Err(e) => results.push(json!({"vault": vault, "path": path, "error": e.to_string()})),
            }
        }
        Ok(json!({ "results": results }))
    }
}

/// Collapses whitespace and cuts to `n` characters, for audit reasons.
fn short_text(s: &str, n: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        format!("{}...", flat.chars().take(n).collect::<String>())
    }
}

/// The lines of a note `read_note` shows (see `Server::note_window`).
struct NoteWindow {
    vault: String,
    path: String,
    sha: String,
    size: usize,
    /// Lines in the note.
    total: usize,
    /// The first line shown, from 1.
    first: usize,
    /// The last line shown (below `first` when none is).
    last: usize,
    /// The last line asked for.
    until: usize,
    /// Byte offset of each line.
    starts: Vec<usize>,
    /// The whole note.
    whole: String,
}

impl NoteWindow {
    /// Whether less than the whole note is shown.
    fn cut(&self) -> bool {
        self.first > 1 || self.last < self.total
    }

    /// The lines shown, as written.
    fn text(&self) -> String {
        if self.last < self.first {
            return String::new();
        }
        let a = self.starts[self.first - 1];
        let b = if self.last < self.total { self.starts[self.last] } else { self.whole.len() };
        self.whole[a..b].to_string()
    }
}
