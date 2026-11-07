//! The listed tool surface: what `tools/list` sends.
//!
//! Every byte of `tools/list` is sent each time a client loads the tools, so
//! the list is a few tools with an `op` argument, each schema naming only
//! the arguments most calls use, without descriptions. Each op runs one of
//! the full tools ([`super::TOOLS`] and the rest), whose names stay callable
//! as aliases and whose schemas still check every argument; the whole
//! reference, every argument described, is one fetched help text
//! ([`help`]: the resource `atlas://help`, or `note` with `op: "help"`).
//! A child module of `tools`, so it shares the tool definitions.

use crate::json;
use crate::json::{Map, Value};

use super::{all_tools, input_schema, ToolDef};
use crate::error::{Error, Result};

/// The help resource's URI.
pub const HELP_URI: &str = "atlas://help";

/// One op of a merged tool: the tool, the op, the full tool it runs.
struct Op {
    tool: &'static str,
    op: &'static str,
    runs: &'static str,
}

const fn op(tool: &'static str, op: &'static str, runs: &'static str) -> Op {
    Op { tool, op, runs }
}

/// Every op of the merged tools, the first of each tool its default where
/// [`DEFAULT_OP`] names one.
const OPS: &[Op] = &[
    op("note", "read", "read_note"),
    op("note", "list", "list_notes"),
    op("note", "find", "find_by_frontmatter"),
    op("note", "backlinks", "backlinks"),
    op("note", "outlinks", "outlinks"),
    op("note", "broken", "broken_links"),
    op("note", "link", "make_link"),
    op("note", "log", "write_log"),
    op("note", "vaults", "list_vaults"),
    op("note", "sources", "list_sources"),
    op("edit", "create", "create_note"),
    op("edit", "append", "append_to_note"),
    op("edit", "history", "append_history"),
    op("edit", "frontmatter", "set_frontmatter"),
    op("edit", "section", "replace_section"),
    op("edit", "link", "link_notes"),
    op("edit", "next_id", "next_id"),
    op("edit", "index", "build_index"),
    op("edit", "refresh", "refresh_index"),
    op("edit", "refresh_all", "refresh_all_indices"),
    op("discuss", "open", "open_discussion"),
    op("discuss", "post", "post"),
    op("discuss", "read", "read_discussion"),
    op("discuss", "status", "discussion_status"),
    op("discuss", "awaiting", "awaiting"),
    op("discuss", "list", "list_discussions"),
    op("discuss", "queue", "decision_queue"),
    op("discuss", "conclude", "conclude_discussion"),
    op("discuss", "unresolved", "mark_unresolved"),
    op("discuss", "decide", "record_decision"),
    op("miss", "report", "report_miss"),
    op("miss", "list", "misses"),
    op("miss", "resolve", "resolve_miss"),
    op("recall", "status", "journal_status"),
    op("recall", "log", "memory_log"),
];

/// The op a merged tool runs when the call names none.
const DEFAULT_OP: &[(&str, &str)] = &[("note", "read"), ("miss", "list")];

fn ops_of(tool: &str) -> Vec<&'static str> {
    OPS.iter().filter(|o| o.tool == tool).map(|o| o.op).collect()
}

fn s() -> Value {
    json!({"type": "string"})
}
fn n() -> Value {
    json!({"type": "integer"})
}
fn b() -> Value {
    json!({"type": "boolean"})
}
fn arr() -> Value {
    json!({"type": "array"})
}
fn o() -> Value {
    json!({"type": "object"})
}
/// Properties from a list of (name, schema).
fn props(p: &[(&str, Value)]) -> Map {
    let mut m = Map::new();
    for (k, v) in p {
        m.insert((*k).to_string(), v.clone());
    }
    m
}
fn schema(p: &[(&str, Value)], required: &[&str]) -> Value {
    let mut v = json!({"type": "object", "properties": Value::Object(props(p))});
    if !required.is_empty() {
        v["required"] = json!(required);
    }
    v
}
fn op_enum(tool: &str) -> Value {
    json!({"type": "string", "enum": ops_of(tool)})
}
fn reason() -> Value {
    s()
}

/// The listed tools. Arguments left out of a schema here are still taken
/// (see the help text).
pub(super) const LISTED: &[ToolDef] = &[
    ToolDef {
        name: "lookup",
        description: "Where an identifier is defined and mentioned; without id, list identifiers.",
        read_only: true,
        schema: || schema(&[("id", s()), ("prefix", s()), ("pattern", s()), ("limit", n()), ("full", b())], &[]),
    },
    ToolDef {
        name: "search",
        description: "Lines in vaults, or ranked sections of every vault and source (sections, semantic, hybrid).",
        read_only: true,
        schema: || {
            schema(
                &[
                    ("query", s()),
                    ("mode", json!({"type": "string", "enum": ["substring", "regex", "fts", "sections", "semantic", "hybrid"]})),
                    ("vault", s()),
                    ("source", s()),
                    ("folder", s()),
                    ("full", b()),
                    ("limit", n()),
                ],
                &["query"],
            )
        },
    },
    ToolDef {
        name: "section",
        description: "One section's exact text by heading or id, with file and lines.",
        read_only: true,
        schema: || schema(&[("heading", s()), ("id", s()), ("source", s()), ("path", s())], &[]),
    },
    ToolDef {
        name: "grep",
        description: "Regex over the indexed vaults and sources, with context lines.",
        read_only: true,
        schema: || schema(&[("pattern", s()), ("source", s()), ("path", s()), ("context", n())], &["pattern"]),
    },
    ToolDef {
        name: "symbol",
        description: "Code symbol: definitions, references with their caller, impls.",
        read_only: true,
        schema: || schema(&[("name", s()), ("limit", n())], &["name"]),
    },
    ToolDef {
        name: "recall",
        description: "Memory by key, query or lane; op status: lanes, op log: versions.",
        read_only: true,
        schema: || {
            schema(
                &[
                    ("key", s()),
                    ("query", s()),
                    ("lane", s()),
                    ("op", op_enum("recall")),
                    ("history", b()),
                    ("limit", n()),
                    ("budget_tokens", n()),
                ],
                &[],
            )
        },
    },
    ToolDef {
        name: "remember",
        description: "Store a memory, rule or decision; revising needs expected_sha; old: supersede.",
        read_only: false,
        schema: || {
            schema(
                &[
                    ("kind", json!({"type": "string", "enum": ["memory", "rule", "decision"]})),
                    ("key", s()),
                    ("title", s()),
                    ("body", s()),
                    ("sub", s()),
                    ("scope", s()),
                    ("refs", arr()),
                    ("expected_sha", s()),
                    ("old", arr()),
                    ("reason", reason()),
                ],
                &["kind", "body", "reason"],
            )
        },
    },
    ToolDef {
        name: "journal",
        description: "Append a lane journal entry.",
        read_only: false,
        schema: || {
            schema(
                &[("lane", s()), ("entry", json!({"type": "string", "enum": crate::memory::ENTRIES})), ("text", s()), ("meta", s()), ("refs", arr())],
                &["lane", "entry", "text"],
            )
        },
    },
    ToolDef {
        name: "handoff",
        description: "Write a lane handoff (allowed past the context cap).",
        read_only: false,
        schema: || schema(&[("lane", s()), ("title", s()), ("text", s()), ("refs", arr())], &["lane", "text"]),
    },
    ToolDef {
        name: "note",
        description: "Vault notes, read only. op help: every op and argument.",
        read_only: true,
        schema: || {
            let mut ops = ops_of("note");
            ops.push("help");
            schema(
                &[
                    ("op", json!({"type": "string", "enum": ops})),
                    ("vault", s()),
                    ("path", s()),
                    ("folder", s()),
                    ("key", s()),
                    ("value", s()),
                    ("lines", s()),
                    ("budget_tokens", n()),
                    ("limit", n()),
                ],
                &[],
            )
        },
    },
    ToolDef {
        name: "edit",
        description: "Write vault notes; never deletes.",
        read_only: false,
        schema: || {
            schema(
                &[
                    ("op", op_enum("edit")),
                    ("vault", s()),
                    ("path", s()),
                    ("content", s()),
                    ("text", s()),
                    ("heading", s()),
                    ("values", o()),
                    ("expected_sha256", s()),
                    ("to_vault", s()),
                    ("to_path", s()),
                    ("folder", s()),
                    ("prefix", s()),
                    ("reason", reason()),
                ],
                &["op"],
            )
        },
    },
    ToolDef {
        name: "discuss",
        description: "Discussion threads and the decision queue. conclude, unresolved, decide: see note op=help.",
        read_only: false,
        schema: || {
            schema(
                &[
                    ("op", op_enum("discuss")),
                    ("thread", s()),
                    ("vault", s()),
                    ("author", s()),
                    ("kind", s()),
                    ("body", s()),
                    ("re", n()),
                    ("topic", s()),
                    ("subjects", arr()),
                    ("participants", arr()),
                    ("opening_post", o()),
                    ("reason", reason()),
                ],
                &["op"],
            )
        },
    },
    ToolDef {
        name: "miss",
        description: "Misses: report what atlas did not return, list them, resolve one.",
        read_only: false,
        schema: || schema(&[("op", op_enum("miss")), ("tool", s()), ("query", s()), ("found", s()), ("id", n()), ("reason", reason())], &[]),
    },
    ToolDef {
        name: "hook",
        description: "Claude Code hook entry point; not for models.",
        read_only: false,
        schema: || schema(&[("event", s())], &["event"]),
    },
];

/// Where a call goes: the full tool that runs it and its arguments, or
/// `None` for the help text. A full tool's own name (an alias) runs as is.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an op the tool does not have, or a merged
/// tool called without one where it has no default.
pub(super) fn route(name: &str, args: &Value) -> Result<Option<(String, Value)>> {
    let Some(m) = args.as_object() else {
        return Ok(Some((name.to_string(), args.clone())));
    };
    let op = m.get("op").and_then(Value::as_str).map(str::to_string);
    let rest = || {
        let mut r = m.clone();
        r.remove("op");
        r
    };
    let runs = match name {
        "lookup" if op.is_none() && !m.contains_key("id") => "ids",
        "remember" if op.is_none() && m.contains_key("old") => "supersede",
        "note" if op.as_deref() == Some("help") => return Ok(None),
        "note" | "edit" | "discuss" | "miss" | "recall" => {
            let op = match (op, DEFAULT_OP.iter().find(|d| d.0 == name)) {
                (Some(o), _) => o,
                (None, Some(d)) => d.1.to_string(),
                (None, None) if name == "recall" => return Ok(Some((name.to_string(), args.clone()))),
                (None, None) => return Err(Error::InvalidArgument(format!("{name} needs op: {}", ops_of(name).join(", ")))),
            };
            match OPS.iter().find(|o| o.tool == name && o.op == op) {
                Some(o) => o.runs,
                None => return Err(Error::InvalidArgument(format!("{name} has no op {op:?}; use {}", ops_of(name).join(", ")))),
            }
        }
        _ => return Ok(Some((name.to_string(), args.clone()))),
    };
    let mut a = rest();
    // `edit` link writes from vault and path, as every other edit names its note.
    if runs == "link_notes" {
        for (from, to) in [("vault", "from_vault"), ("path", "from_path")] {
            if let Some(v) = a.remove(from) {
                a.insert(to.to_string(), v);
            }
        }
    }
    // `note` link takes the note's vault as the link's source vault by default.
    if runs == "make_link" {
        if let Some(v) = a.remove("vault") {
            if !a.contains_key("from_vault") {
                a.insert("from_vault".to_string(), v.clone());
            }
            if !a.contains_key("to_vault") {
                a.insert("to_vault".to_string(), v);
            }
        }
    }
    Ok(Some((runs.to_string(), Value::Object(a))))
}

/// The help text: every listed tool with its ops, then every full tool with
/// its description and every argument described.
pub fn help() -> String {
    let mut out = String::from(
        "# atlas tools\n\nAnswers are plain text; `format: \"json\"` gives JSON. A repeated read answers `unchanged since ... #N`, or only the lines that changed: per agent with the hooks on, per server without them (the earlier call may be another agent's); `fresh: true` gives the whole answer. `read_note` takes `lines` (`A-B`) or `budget_tokens`, on a vault or a source. Each op runs the full tool named after it, \
         and every argument of that tool is accepted (the listed schemas name the common ones). The full tool names also \
         work as tool names, for one release.\n\n## Listed tools\n\n",
    );
    for t in LISTED {
        out.push_str(&format!("- `{}`: {}\n", t.name, t.description));
        for o in OPS.iter().filter(|o| o.tool == t.name) {
            out.push_str(&format!("  - op `{}` runs `{}`\n", o.op, o.runs));
        }
        match t.name {
            "lookup" => out.push_str("  - without `id` runs `ids`\n"),
            "remember" => out.push_str("  - with `old` runs `supersede`\n"),
            "edit" => out.push_str("  - op `link`: `vault` and `path` are the note the link is written into\n"),
            "journal" => out.push_str(
                "  - entry `queued` files planned work, as `atlas queue plan` does: the text's first line is the title, the rest its scope; `meta` lines `kind:`, `priority:`, `uses:`, `after:` (queue keys or free text) and `order:` are checked, other lines kept\n  - entry `review` files a review for the merge gate (`orchestrate.reviews`): `meta` lines `role:`, `pr:`, `head:` (the sha read), `verdict:` (pass, merge, merge with follow-ups, request changes) and `report:`\n  - `amend: KEY` with `expected_sha` (the head's sha) rewrites that entry's title and body as a new version; its lane, entry, meta lines and refs stay (pass the same `lane` and `entry`, no `meta` or `refs`)\n",
            ),
            _ => {}
        }
    }
    out.push_str("\n## Full tools\n");
    for t in all_tools() {
        out.push_str(&format!("\n### {}\n\n{}{}\n\n", t.name, t.description, if t.read_only { "" } else { " (writes)" }));
        let sc = input_schema(t);
        let req: Vec<&str> = sc["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if let Some(p) = sc["properties"].as_object() {
            for (k, v) in p.iter() {
                let ty = v["type"].as_str().unwrap_or("any");
                let en = v["enum"].as_array().map(|e| format!(" ({})", e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))).unwrap_or_default();
                let d = v["description"].as_str().map(|d| format!(": {d}")).unwrap_or_default();
                let r = if req.contains(&k.as_str()) { ", required" } else { "" };
                out.push_str(&format!("- `{k}` {ty}{en}{r}{d}\n"));
            }
        }
    }
    out
}

/// Every op of the merged tools: (tool, op, the full tool it runs).
pub fn ops() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    OPS.iter().map(|o| (o.tool, o.op, o.runs))
}
