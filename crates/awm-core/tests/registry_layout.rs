//! Unit tests for the registry model and the pure layout engine.

use awm_core::{plan_layout, LayoutMode, Registry};
use awm_proto::{AgentEvent, AgentId, AgentMeta, AgentState, ApprovalCtx, LayoutCmd, LineKind, TokenUsage};

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
fn terminal_agent_absorbs_late_events_without_tail_noise() {
    let (mut reg, ids) = reg_with(1);
    reg.apply_event(ids[0], &AgentEvent::Finished { ok: true });
    let tail_before = reg.record(ids[0]).unwrap().tail.len();
    // The reader's EOF safety-net (`Finished{ok:false}`) after a clean finish
    // must be fully absorbed — no state change, no spurious "failed" tail line.
    reg.apply_event(ids[0], &AgentEvent::Finished { ok: false });
    let rec = reg.record(ids[0]).unwrap();
    assert_eq!(rec.state, AgentState::Done);
    assert_eq!(rec.tail.len(), tail_before);
}

#[test]
fn streamed_deltas_grow_one_line_and_finalize_without_duplication() {
    let (mut reg, ids) = reg_with(1);
    let a = ids[0];
    reg.apply_event(a, &AgentEvent::Started { model: "m".into(), cwd: "/".into() });
    let base = reg.record(a).unwrap().tail.len();

    // Deltas (interleaved with the Noise the stream emits between blocks).
    for chunk in ["Hel", "lo ", "world"] {
        reg.apply_event(a, &AgentEvent::Noise); // e.g. content_block_start/stop
        reg.apply_event(a, &AgentEvent::MessageDelta { text: chunk.into() });
    }
    // Exactly one new (live) line that has grown.
    assert_eq!(reg.record(a).unwrap().tail.len(), base + 1);
    let live = reg.record(a).unwrap().tail.back().unwrap();
    assert_eq!(live.kind, LineKind::Text);
    assert_eq!(live.text, "Hello world");

    // The complete message finalizes it — no duplicate line appended.
    reg.apply_event(a, &AgentEvent::Message { text: "Hello world".into() });
    assert_eq!(reg.record(a).unwrap().tail.len(), base + 1);
    assert_eq!(reg.record(a).unwrap().tail.back().unwrap().text, "Hello world");
}

#[test]
fn set_permission_mode_updates_view_even_without_prior_info() {
    let (mut reg, ids) = reg_with(1);
    reg.set_permission_mode(ids[0], "plan");
    let view = reg.views().into_iter().next().unwrap();
    assert_eq!(view.info.unwrap().permission_mode, "plan");
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
    // One transcript line per ToolStarted; push well past the cap.
    for i in 0..1000 {
        reg.apply_event(
            ids[0],
            &AgentEvent::ToolStarted {
                name: format!("t{i}"),
                summary: String::new(),
            },
        );
    }
    let n = reg.record(ids[0]).unwrap().tail.len();
    assert!(n <= 400, "tail should be capped, was {n}");
}

// ---- Projects (screens) -----------------------------------------------------

#[test]
fn views_and_focus_are_scoped_to_active_project() {
    let mut reg = Registry::new();
    let p_default = reg.active();

    // Two agents on the default project.
    let a0 = reg.alloc_id();
    reg.add(AgentMeta::new(a0, "a0", "/tmp".into(), 0));
    let a1 = reg.alloc_id();
    reg.add(AgentMeta::new(a1, "a1", "/tmp".into(), 0));

    // A second project with one agent.
    let p_web = reg.add_project("web", "/site".into());
    reg.set_active(p_web);
    let b0 = reg.alloc_id();
    reg.add(AgentMeta::new(b0, "b0", "/site".into(), 0));

    // Active = web → only b0 is visible; focus is web's.
    assert_eq!(reg.views().iter().map(|v| v.meta.id).collect::<Vec<_>>(), vec![b0]);
    assert_eq!(reg.focus(), Some(b0));
    assert_eq!(reg.active_order(), vec![b0]);

    // Switch back to default → a0, a1 visible again; default's focus preserved.
    reg.set_active(p_default);
    assert_eq!(reg.views().iter().map(|v| v.meta.id).collect::<Vec<_>>(), vec![a0, a1]);
    assert_eq!(reg.focus(), Some(a0), "default project kept its own focus");
}

#[test]
fn switch_to_is_one_based_and_ignores_out_of_range() {
    let mut reg = Registry::new(); // project 1 = default
    let p2 = reg.add_project("two", "/two".into());
    reg.switch_to(2);
    assert_eq!(reg.active(), p2);
    reg.switch_to(9); // no such project
    assert_eq!(reg.active(), p2, "out-of-range switch is a no-op");
}

#[test]
fn blocked_ordered_only_sees_active_project() {
    let mut reg = Registry::new();
    let p_default = reg.active();
    let a = reg.alloc_id();
    reg.add(AgentMeta::new(a, "a", "/tmp".into(), 0));

    let p_web = reg.add_project("web", "/site".into());
    reg.set_active(p_web);
    let b = reg.alloc_id();
    reg.add(AgentMeta::new(b, "b", "/site".into(), 0));

    // Block the default-project agent while `web` is active.
    reg.apply_event(a, &AgentEvent::ApprovalRequested(ctx("req_a", "Bash")));

    // web is active → no blocked agent here, but the default tab is urgent.
    assert!(reg.blocked_ordered().is_empty(), "active project has no block");
    assert!(reg.project_is_urgent(p_default), "background project flags urgent");
    assert!(!reg.project_is_urgent(p_web));

    // Switch to default → the block surfaces for triage/answering.
    reg.set_active(p_default);
    assert_eq!(reg.blocked_ordered(), vec![a]);
}

#[test]
fn urgent_to_master_is_scoped_to_active_project() {
    let mut reg = Registry::new();
    let a = reg.alloc_id();
    reg.add(AgentMeta::new(a, "a", "/tmp".into(), 0));
    let p_web = reg.add_project("web", "/site".into());
    reg.set_active(p_web);
    let b = reg.alloc_id();
    reg.add(AgentMeta::new(b, "b", "/site".into(), 0));

    // A block on the background (default) agent must NOT hijack web's master zone.
    reg.apply_event(a, &AgentEvent::ApprovalRequested(ctx("req_a", "Bash")));
    assert_eq!(plan_layout(&reg, LayoutMode::Tiling), LayoutCmd::SetMaster(b));

    // On the default screen, the blocked agent takes master (urgent → master).
    reg.set_active(reg.projects()[0].id);
    assert_eq!(plan_layout(&reg, LayoutMode::Tiling), LayoutCmd::SetMaster(a));
}

// ---- Persistence (snapshot / restore) ---------------------------------------

#[test]
fn snapshot_restore_round_trips_projects_panes_and_active() {
    use awm_core::SessionState;

    let mut reg = Registry::new();
    let p_default = reg.active();
    reg.set_project_meta(p_default, "awm", "/home/dev/awm".into());

    // Default project: one agent with some transcript + state.
    let a = reg.alloc_id();
    reg.add(AgentMeta::new(a, "builder", "/home/dev/awm".into(), 0));
    reg.apply_event(a, &AgentEvent::Started { model: "m".into(), cwd: "/home/dev/awm".into() });
    reg.apply_event(a, &AgentEvent::ToolStarted { name: "Bash".into(), summary: "cargo build".into() });

    // A second project with its own agent, and make it active.
    let p_web = reg.add_project("web", "/home/dev/web".into());
    reg.set_active(p_web);
    let b = reg.alloc_id();
    reg.add(AgentMeta::new(b, "server", "/home/dev/web".into(), 0));

    // Round-trip through JSON.
    let json = serde_json::to_string(&reg.snapshot()).unwrap();
    let state: SessionState = serde_json::from_str(&json).unwrap();

    let mut restored = Registry::new();
    restored.restore(&state);

    // Projects and active preserved.
    let names: Vec<&str> = restored.projects().iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["awm", "web"]);
    assert_eq!(restored.active(), p_web, "active project preserved");

    // Active (web) shows only its agent, with the same id.
    let web_ids: Vec<AgentId> = restored.views().iter().map(|v| v.meta.id).collect();
    assert_eq!(web_ids, vec![b]);

    // Switch to the default project: the builder pane + its transcript survived.
    restored.set_active(p_default);
    let builder = restored.record(a).unwrap();
    assert_eq!(builder.meta.name, "builder");
    assert!(builder.tail.iter().any(|l| l.text.contains("session started")));
    assert!(builder.tail.iter().any(|l| l.text.contains("Bash")));

    // Fresh ids never collide with restored ones.
    let c = restored.alloc_id();
    assert!(c.0 > a.0 && c.0 > b.0, "next id advanced past restored agents");
}

#[test]
fn remove_project_drops_its_panes_and_switches_active() {
    let mut reg = Registry::new();
    let p_default = reg.active();
    let a = reg.alloc_id();
    reg.add(AgentMeta::new(a, "a", "/tmp".into(), 0));

    let p_web = reg.add_project("web", "/site".into());
    reg.set_active(p_web);
    let b = reg.alloc_id();
    reg.add(AgentMeta::new(b, "b", "/site".into(), 0));

    // Close the active (web) project.
    assert!(reg.remove_project(p_web));
    assert_eq!(reg.projects().len(), 1, "web project removed");
    assert_eq!(reg.active(), p_default, "switched to the neighbour");
    assert!(reg.record(b).is_none(), "web's agent pane dropped");
    assert_eq!(reg.views().iter().map(|v| v.meta.id).collect::<Vec<_>>(), vec![a]);
}

#[test]
fn cannot_close_the_last_project() {
    let mut reg = Registry::new();
    let only = reg.active();
    assert!(!reg.remove_project(only), "the sole screen can't be closed");
    assert_eq!(reg.projects().len(), 1);
    assert_eq!(reg.active(), only);
}
