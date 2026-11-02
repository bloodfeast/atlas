//! The intake tools (settings, grammar and write-side records). A child
//! module of `tools`, so it shares the server's private state.

use crate::json::Map;

use super::{Answer, Fmt, Server, ToolDef};
use crate::error::Result;

/// The tools this module serves, appended to the tool list.
pub(super) const TOOLS: &[ToolDef] = &[];

impl Server {
    /// Runs one of [`TOOLS`], or `None` when `name` is not one of them.
    pub(super) fn run_intake(&mut self, name: &str, a: &Map, f: Fmt) -> Option<Result<Answer>> {
        let _ = (name, a, f);
        None
    }
}
