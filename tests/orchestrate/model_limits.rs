//! #403: per-model context limits (`orchestrate.kinds.<kind>.models`)
//! through the PreToolUse cap. Part of `tests/orchestrate.rs` (its helpers).

use super::*;
use atlas::orchestrate::ModelSpec;

/// Rewrites agent `id`'s transcript so its last assistant message names
/// `model` (None: no model, so the meta file's alias is read).
fn set_model(s: &Session, id: &str, brief: &str, ctx: u64, model: Option<&str>, alias: &str) {
    let meta = json!({"agentType": "general-purpose", "isBuiltIn": true, "description": format!("job {id}"), "toolUseId": format!("t-{id}"), "model": alias});
    std::fs::write(s.dir.join(format!("agent-{id}.meta.json")), meta.to_string()).unwrap();
    let user = json!({"type": "user", "message": {"role": "user", "content": brief}});
    let mut msg = json!({"id": "m1", "usage": {"input_tokens": 2, "cache_creation_input_tokens": 0, "cache_read_input_tokens": ctx - 2, "output_tokens": 0}});
    if let (Some(m), Value::Object(o)) = (model, &mut msg) {
        o.insert("model".into(), Value::String(m.into()));
    }
    let usage = json!({"type": "assistant", "message": msg});
    std::fs::write(s.dir.join(format!("agent-{id}.jsonl")), format!("{user}\n{usage}\n")).unwrap();
}

#[test]
fn a_model_entry_sets_the_points_and_the_hook_names_it() {
    let t = Temp::new("orch-model-limits");
    let s = Session::new(t.path());
    let entry = |limit: Option<usize>, handoff: Option<usize>| ModelSpec { limit, handoff, ..Default::default() };
    let o = OrchestrateSpec {
        enabled: Some(true),
        kinds: vec![(
            Kind::Builder,
            KindSpec {
                models: vec![("haiku".into(), entry(Some(180_000), Some(90_000))), ("claude-haiku-5-5".into(), entry(None, Some(60_000)))],
                ..Default::default()
            },
        )],
        ..Default::default()
    };
    let mut h = Hooks::open(settings(t.path(), &o, &[])).unwrap();
    let brief = "Kind: builder\nLane: X\nBuild it.";
    // An alias entry for another haiku id, from the transcript's model.
    set_model(&s, "h4", brief, 95_000, Some("claude-haiku-4-5"), "haiku");
    // An exact id beats the alias; its limit is the kind's (450k).
    set_model(&s, "h5", brief, 70_000, Some("claude-haiku-5-5"), "haiku");
    // No model in the transcript yet: the meta file's alias; past the hard limit.
    set_model(&s, "hm", brief, 190_000, None, "haiku");
    // A model with no entry: the kind's own points and text, as before.
    set_model(&s, "op", brief, 95_000, Some("claude-opus-5-5"), "opus");
    set_model(&s, "op2", brief, 300_000, Some("claude-opus-5-5"), "opus");
    let call = |h: &mut Hooks, agent: &str| {
        h.handle(&HookEvent { agent_id: agent.into(), agent_type: "general-purpose".into(), tool_name: "Bash".into(), transcript_path: s.tp(), ..ev("PreToolUse") })
    };
    let a = call(&mut h, "h4");
    assert!(
        a.deny.is_none() && a.context.contains("handoff point 90000, hard limit 180000, haiku builder: 90,000 (orchestrate.kinds.builder.models.haiku)"),
        "{a:?}"
    );
    let a = call(&mut h, "h5");
    assert!(
        a.deny.is_none() && a.context.contains("handoff point 60000, hard limit 450000, claude-haiku-5-5 builder: 60,000 (orchestrate.kinds.builder.models.claude-haiku-5-5)"),
        "{a:?}"
    );
    let a = call(&mut h, "hm");
    assert!(a.deny.as_deref().is_some_and(|r| r.contains("hard limit 180000, haiku builder: 180,000 (orchestrate.kinds.builder.models.haiku)")), "{a:?}");
    assert_eq!(call(&mut h, "op"), HookAnswer::default(), "under the kind's 285k handoff");
    let a = call(&mut h, "op2");
    assert!(a.deny.is_none() && a.context.contains("(handoff point 285000, hard limit 450000, builder limits)"), "{a:?}");

    // A resume past a model entry's hard limit is refused and names it.
    let r = h.handle(&HookEvent { tool_name: "SendMessage".into(), transcript_path: s.tp(), tool_input: json!({"to": "hm", "message": "more"}), ..ev("PreToolUse") });
    assert!(r.deny.as_deref().is_some_and(|d| d.contains("past its hard limit 180000 (haiku builder: 180,000") && d.contains("Do not resume it")), "{r:?}");
}
