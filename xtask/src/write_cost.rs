//! `cargo xtask write-cost`: what a project tracking write costs the model in
//! context, against the same write through the Atlassian MCP.
//!
//! The cases are the golden `fixtures/atlassian/mcp-replies/size-cases.json`
//! (written by the `the_size_table_golden_matches_the_code` test from the real
//! write code, so the atlas side is never typed by hand). Per case it counts
//! what the model sends (the tool input) and receives (the reply) for atlas
//! dry-run, atlas `--apply` and the MCP, in bytes and in tokens (`bpe::tokens`).
//! The MCP side is synthetic: the documented response shapes, not recorded
//! replies.

use atlas::json::{self, Value};

use crate::bpe;

/// The golden, relative to the atlas crate root.
pub const GOLDEN: &str = "fixtures/atlassian/mcp-replies/size-cases.json";

/// Bytes and tokens of one piece of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub bytes: usize,
    pub tokens: usize,
}

fn size<'a>(parts: impl IntoIterator<Item = &'a str>) -> Size {
    parts.into_iter().fold(Size { bytes: 0, tokens: 0 }, |s, p| Size { bytes: s.bytes + p.len(), tokens: s.tokens + bpe::tokens(p) })
}

/// One write's sizes.
#[derive(Clone, Debug)]
pub struct Row {
    pub name: String,
    pub atlas_in: Size,
    pub dry_out: Size,
    pub apply_out: Size,
    pub mcp_in: Size,
    pub mcp_out: Size,
}

fn strings(v: &Value, key: &str) -> Result<Vec<String>, String> {
    v.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{key}: not a list"))?
        .iter()
        .map(|s| s.as_str().map(str::to_string).ok_or_else(|| format!("{key}: not text")))
        .collect()
}

fn text(v: &Value, key: &str) -> Result<String, String> {
    v.get(key).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("{key}: missing"))
}

/// The rows of a golden's text.
///
/// # Errors
///
/// A golden that is not the expected shape.
pub fn rows(golden: &str) -> Result<Vec<Row>, String> {
    let v = json::parse(golden).map_err(|e| format!("size-cases: {e:?}"))?;
    let cases = v.get("cases").and_then(Value::as_array).ok_or("size-cases: no cases")?;
    cases
        .iter()
        .map(|c| {
            let mcp_in = strings(c, "mcp_in")?;
            let mcp_out = strings(c, "mcp_out")?;
            Ok(Row {
                name: text(c, "name")?,
                atlas_in: size([text(c, "atlas_in")?.as_str()]),
                dry_out: size([text(c, "atlas_dry")?.as_str()]),
                apply_out: size([text(c, "atlas_apply")?.as_str()]),
                mcp_in: size(mcp_in.iter().map(String::as_str)),
                mcp_out: size(mcp_out.iter().map(String::as_str)),
            })
        })
        .collect()
}

/// The table as Markdown: bytes / tokens per cell, atlas dry-run and apply
/// against the MCP, input then reply.
#[must_use]
pub fn table(rows: &[Row]) -> String {
    let cell = |s: Size| format!("{} B / {} tok", s.bytes, s.tokens);
    let mut out = String::from(
        "| write | atlas sends | MCP sends | atlas dry-run reply | atlas --apply reply | MCP reply |\n|---|---|---|---|---|---|\n",
    );
    for r in rows {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            r.name,
            cell(r.atlas_in),
            cell(r.mcp_in),
            cell(r.dry_out),
            cell(r.apply_out),
            cell(r.mcp_out)
        ));
    }
    out
}

/// What the task prints.
///
/// # Errors
///
/// A golden that cannot be read or parsed.
pub fn run() -> Result<(), String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(GOLDEN);
    let golden = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    print!("{}", table(&rows(&golden)?));
    println!("basis: documented response shapes as synthetic fixtures; swap in recorded replies from a live run when Brandon has them");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden() -> Vec<Row> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(GOLDEN);
        rows(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn an_applied_write_replies_in_less_than_the_mcp_for_all_four() {
        let rows = golden();
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["comment", "transition", "edit", "page_update"]);
        for r in &rows {
            assert!(r.apply_out.tokens < r.mcp_out.tokens, "{}: atlas {:?} against the MCP {:?}", r.name, r.apply_out, r.mcp_out);
        }
    }

    #[test]
    fn a_section_update_sends_less_than_the_mcp_page_update() {
        let rows = golden();
        let p = rows.iter().find(|r| r.name == "page_update").unwrap();
        assert!(p.atlas_in.tokens < p.mcp_in.tokens, "{p:?}");
    }

    #[test]
    fn the_table_has_a_row_per_write() {
        let t = table(&golden());
        assert_eq!(t.lines().count(), 6, "{t}");
    }
}
