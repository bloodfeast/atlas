//! The `hook` tool's SessionStart, on the route `tools_memory` answers. Its own
//! test binary, because that route finds Claude Code's settings through
//! `CLAUDE_CONFIG_DIR`, which is process-wide (as in `chain_keys.rs`).

mod common;

use common::*;

/// The `hook` tool does not say a user is there, so SessionStart asks nothing
/// and spends nothing, on the route `tools_memory` answers.
#[test]
fn the_hook_tool_asks_nothing_at_session_start() {
    let fx = Fx::new("p-hook-start");
    // Not the developer's own Claude folder: SessionStart points the status line it
    // finds at the running binary, this test binary.
    std::env::set_var("CLAUDE_CONFIG_DIR", fx.tmp.path().join("claude"));
    let mut s = fx.server();
    let state = fx.tmp.path().join("mem");
    s.set_memory(atlas::memory::settings::Settings::from_config(None, &state.join("index.sqlite")).unwrap());
    let input = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"hook","arguments":{"event":"SessionStart","session_id":"s","source":"startup"}}}
"#;
    let mut out = Vec::new();
    atlas::protocol::serve(&mut s, &input[..], &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.contains("\"id\":7") && !out.contains("On by default") && !out.contains("Claude Code setting"), "{out}");
    assert!(!state.join(atlas::claude_settings::OFFER_STATE).exists(), "nothing spent: {out}");
}
