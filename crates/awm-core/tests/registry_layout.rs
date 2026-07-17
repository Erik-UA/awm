//! Unit tests for the registry model and the pure layout engine.

use awm_core::{plan_layout, LayoutMode, Registry};
use awm_proto::{AgentEvent, AgentId, AgentMeta, AgentState, ApprovalCtx, LayoutCmd, TokenUsage};

fn ctx(request_id: &str, tool: &str) -> ApprovalCtx {
    ApprovalCtx {
        tool: tool.into(),
        input: serde_json::json!({"x": 1}),
        request_id: request_id.into(),
        tool_use_id: Some("toolu_1".into()),
        description: Some("do a thing".into()),
        decision_reason: None,
        diff: None,
    }
}

fn reg_with(n: u32) -> (Registry, Vec<AgentId>) {
    let mut reg = Registry::new();
    let mut ids = Vec::new();
    for i in 0..n {
        let id = reg.alloc_id();
        reg.add(AgentMeta::new(id, format!("a{i}"), "/tmp".into(), 0));
        ids.push(id);
    }
    (reg, ids)
}

#[test]
fn lifecycle_through_approval_updates_state_and_pending() {
    let (mut reg, ids) = reg_with(1);
    let a = ids[0];

    reg.apply_event(a, &AgentEvent::Started { model: "m".into(), cwd: "/tmp".into() });
    assert_eq!(reg.record(a).unwrap().state, AgentState::Working);

    reg.apply_event(a, &AgentEvent::ApprovalRequested(ctx("req_1", "Bash")));
    let rec = reg.record(a).unwrap();
    assert_eq!(rec.state, AgentState::BlockedOnApproval);
    assert!(rec.meta.urgent);
    assert!(rec.blocked_since.is_some());
    assert_eq!(reg.pending_request_id(a).as_deref(), Some("req_1"));

    reg.apply_event(a, &AgentEvent::ApprovalResolved { approved: true });
    let rec = reg.record(a).unwrap();
    assert_eq!(rec.state, AgentState::Working);
    assert!(!rec.meta.urgent);
    assert!(rec.blocked_since.is_none());
    assert!(reg.pending_request_id(a).is_none());

    reg.apply_event(a, &AgentEvent::Finished { ok: true });
    assert_eq!(reg.record(a).unwrap().state, AgentState::Done);
    assert!(reg.all_terminal());
}

#[test]
fn tokens_are_recorded() {
    let (mut reg, ids) = reg_with(1);
    reg.apply_event(ids[0], &AgentEvent::Tokens(TokenUsage { input: 100, output: 20 }));
    assert_eq!(reg.record(ids[0]).unwrap().tokens.total(), 120);
}

#[test]
fn urgent_promotes_oldest_blocked_to_master() {
    let (mut reg, ids) = reg_with(3);
    // Block a2 first, then a0 — a2 waited longer, so it must win master.
    reg.apply_event(ids[2], &AgentEvent::ApprovalRequested(ctx("r2", "Bash")));
    reg.apply_event(ids[0], &AgentEvent::ApprovalRequested(ctx("r0", "Edit")));

    assert_eq!(reg.blocked_ordered(), vec![ids[2], ids[0]]);
    assert_eq!(plan_layout(&reg, LayoutMode::Tiling), LayoutCmd::SetMaster(ids[2]));
}

#[test]
fn tiling_without_blocks_uses_focus() {
    let (mut reg, ids) = reg_with(3);
    reg.set_focus(ids[1]);
    assert_eq!(plan_layout(&reg, LayoutMode::Tiling), LayoutCmd::SetMaster(ids[1]));
}

#[test]
fn triage_lists_blocked_oldest_first_else_falls_back() {
    let (mut reg, ids) = reg_with(3);
    // No blocks yet → triage falls back to tiling (master = focus/first).
    assert!(matches!(plan_layout(&reg, LayoutMode::Triage), LayoutCmd::SetMaster(_)));

    reg.apply_event(ids[1], &AgentEvent::ApprovalRequested(ctx("r1", "Bash")));
    reg.apply_event(ids[0], &AgentEvent::ApprovalRequested(ctx("r0", "Bash")));
    assert_eq!(
        plan_layout(&reg, LayoutMode::Triage),
        LayoutCmd::Triage(vec![ids[1], ids[0]])
    );
}

#[test]
fn monocle_targets_focus() {
    let (mut reg, ids) = reg_with(2);
    reg.set_focus(ids[1]);
    assert_eq!(plan_layout(&reg, LayoutMode::Monocle), LayoutCmd::Monocle(ids[1]));
}

#[test]
fn focus_step_wraps() {
    let (mut reg, ids) = reg_with(3);
    reg.set_focus(ids[0]);
    reg.focus_step(-1);
    assert_eq!(reg.focus(), Some(ids[2])); // wrapped backwards
    reg.focus_step(1);
    assert_eq!(reg.focus(), Some(ids[0]));
}

#[test]
fn tail_ring_is_bounded() {
    let (mut reg, ids) = reg_with(1);
    // describe() emits a line per ToolStarted; push well past the cap.
    for i in 0..500 {
        reg.apply_event(ids[0], &AgentEvent::ToolStarted { name: format!("t{i}") });
    }
    let n = reg.record(ids[0]).unwrap().tail.len();
    assert!(n <= 200, "tail should be capped, was {n}");
}
