//! Headless end-to-end test: drive the real `Engine` (runtime + pty split +
//! parser + layout) with several `mock-agent.py` processes — NEVER live
//! `claude`. Proves the killer-feature loop: agents block on approval, the
//! oldest is promoted to master, we answer over the control channel, and they
//! resume to Done. Also proves the reader-thread/answerer split doesn't deadlock.

use awm_core::{plan_layout, Engine, LayoutMode};
use awm_pty::{CommandSpec, Decision};
use awm_proto::{AgentId, AgentState, LayoutCmd, Tags};
use std::path::PathBuf;
use std::time::Duration;

fn script_spec(file: &str) -> CommandSpec {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(file);
    CommandSpec::new("python3", std::env::temp_dir()).arg(script.to_str().unwrap())
}

fn mock_spec() -> CommandSpec {
    script_spec("mock-agent.py")
}

fn pump_until(engine: &mut Engine, mut done: impl FnMut(&Engine) -> bool) -> bool {
    for _ in 0..200 {
        engine.pump_blocking(Duration::from_millis(200));
        if done(engine) {
            return true;
        }
    }
    false
}

#[test]
fn agents_block_promote_to_master_then_resume_on_approve() {
    let mut engine = Engine::new();
    let ids: Vec<AgentId> = ["a", "b", "c"]
        .iter()
        .map(|n| engine.spawn(mock_spec(), *n, Tags::empty(), None, false).unwrap())
        .collect();

    // All three reach the approval gate.
    let blocked = pump_until(&mut engine, |e| {
        ids.iter()
            .all(|id| e.registry().pending_request_id(*id).is_some())
    });
    assert!(blocked, "all agents should reach BlockedOnApproval");
    for id in &ids {
        assert_eq!(
            engine.registry().record(*id).unwrap().state,
            AgentState::BlockedOnApproval
        );
    }

    // urgent → master: the oldest-waiting blocked agent takes the master zone.
    let order = engine.registry().blocked_ordered();
    assert_eq!(order.len(), 3);
    assert_eq!(
        plan_layout(engine.registry(), LayoutMode::Tiling),
        LayoutCmd::SetMaster(order[0])
    );

    // Approve every gate over the control channel (UI thread answers while the
    // reader threads are blocked in read()).
    for id in &ids {
        engine.answer(*id, Decision::Allow).unwrap();
    }

    // Each agent resumes and finishes successfully.
    let finished = pump_until(&mut engine, |e| e.registry().all_terminal());
    assert!(finished, "all agents should finish after approval");
    for id in &ids {
        assert_eq!(engine.registry().record(*id).unwrap().state, AgentState::Done);
    }

    engine.join();
}

#[test]
fn dead_agent_becomes_failed_not_stuck() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(script_spec("mock-die.py"), "dying", Tags::empty(), None, false)
        .unwrap();

    let terminal = pump_until(&mut engine, |e| {
        e.registry()
            .record(id)
            .map(|r| r.state.is_terminal())
            .unwrap_or(false)
    });
    assert!(terminal, "a crashed agent must reach a terminal state, not hang");
    assert_eq!(engine.registry().record(id).unwrap().state, AgentState::Failed);

    engine.join();
}

#[test]
fn scale_twelve_agents_block_and_resume() {
    let mut engine = Engine::new();
    let ids: Vec<AgentId> = (0..12)
        .map(|i| {
            engine
                .spawn(mock_spec(), format!("a{i}"), Tags::empty(), None, false)
                .unwrap()
        })
        .collect();

    let blocked = pump_until(&mut engine, |e| {
        ids.iter()
            .all(|id| e.registry().pending_request_id(*id).is_some())
    });
    assert!(blocked, "all 12 agents should block");
    // urgent → master still picks a single (oldest) blocked agent.
    let order = engine.registry().blocked_ordered();
    assert_eq!(order.len(), 12);
    assert_eq!(
        plan_layout(engine.registry(), LayoutMode::Tiling),
        LayoutCmd::SetMaster(order[0])
    );

    for id in &ids {
        engine.answer(*id, Decision::Allow).unwrap();
    }
    let finished = pump_until(&mut engine, |e| e.registry().all_terminal());
    assert!(finished, "all 12 agents should finish after approval");
    engine.join();
}

#[test]
fn noisy_stream_still_reaches_done() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(script_spec("mock-noisy.py"), "noisy", Tags::empty(), None, false)
        .unwrap();

    let blocked = pump_until(&mut engine, |e| e.registry().pending_request_id(id).is_some());
    assert!(blocked, "agent should block despite garbage lines in the stream");
    engine.answer(id, Decision::Allow).unwrap();

    let finished = pump_until(&mut engine, |e| e.registry().all_terminal());
    assert!(finished, "agent should finish despite noise");
    assert_eq!(engine.registry().record(id).unwrap().state, AgentState::Done);
    engine.join();
}

#[test]
fn killing_a_blocked_agent_makes_it_terminal() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(mock_spec(), "victim", Tags::empty(), None, false)
        .unwrap();

    // Let it reach the gate (it then blocks forever waiting on stdin).
    assert!(pump_until(&mut engine, |e| e
        .registry()
        .pending_request_id(id)
        .is_some()));

    engine.kill(id);

    let terminal = pump_until(&mut engine, |e| {
        e.registry()
            .record(id)
            .map(|r| r.state.is_terminal())
            .unwrap_or(false)
    });
    assert!(terminal, "a killed agent must become terminal, not hang");
    assert_eq!(engine.registry().record(id).unwrap().state, AgentState::Failed);
    engine.join();
}

#[test]
fn denying_makes_the_agent_fail() {
    let mut engine = Engine::new();
    let id = engine.spawn(mock_spec(), "d", Tags::empty(), None, false).unwrap();

    assert!(pump_until(&mut engine, |e| e
        .registry()
        .pending_request_id(id)
        .is_some()));

    engine.answer(id, Decision::Deny("nope".into())).unwrap();

    // The mock exits non-zero on deny; the agent leaves the blocked state.
    let resolved = pump_until(&mut engine, |e| {
        e.registry().pending_request_id(id).is_none()
    });
    assert!(resolved, "deny should clear the pending approval");
    assert_ne!(
        engine.registry().record(id).unwrap().state,
        AgentState::BlockedOnApproval
    );

    engine.join();
}
