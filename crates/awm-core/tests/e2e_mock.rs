//! Headless end-to-end test: drive the real `Engine` (runtime + pty split +
//! parser + layout) with several `mock-agent.py` processes — NEVER live
//! `claude`. Proves the killer-feature loop: agents block on approval, the
//! oldest is promoted to master, we answer over the control channel, and they
//! resume to Done. Also proves the reader-thread/answerer split doesn't deadlock.

use awm_core::{plan_layout, AgentSnapshot, Engine, LayoutMode, Project, ProjectId, SessionState};
use awm_pty::{CommandSpec, Decision};
use awm_proto::{
    AgentId, AgentMeta, AgentState, LayoutCmd, LineKind, TokenUsage, TranscriptLine, Tags,
};
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
        .map(|n| engine.spawn(mock_spec(), *n, Tags::empty(), None, false, false).unwrap())
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
        .spawn(script_spec("mock-die.py"), "dying", Tags::empty(), None, false, false)
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
                .spawn(mock_spec(), format!("a{i}"), Tags::empty(), None, false, false)
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
        .spawn(script_spec("mock-noisy.py"), "noisy", Tags::empty(), None, false, false)
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
fn multi_turn_dialogue_shows_replies_in_the_window() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(
            script_spec("mock-chat.py"),
            "chat",
            Tags::empty(),
            Some("hello".into()),
            false,
            false,
        )
        .unwrap();

    let tail_has = |e: &Engine, needle: &str| {
        e.registry()
            .record(id)
            .map(|r| r.tail.iter().any(|l| l.text.contains(needle)))
            .unwrap_or(false)
    };

    // First turn: the spawn prompt "hello" gets echoed back.
    assert!(pump_until(&mut engine, |e| tail_has(e, "echo: hello")));

    // Second turn: send a follow-up to the LIVE agent, see its reply.
    engine.send_message(id, "how are you").unwrap();
    assert!(pump_until(&mut engine, |e| tail_has(e, "echo: how are you")));

    // The user's own line is echoed into the window too (dialogue view).
    assert!(tail_has(&engine, "you: how are you"));
    // Still the same live session — not a new agent, not terminal.
    assert!(!engine.registry().record(id).unwrap().state.is_terminal());

    engine.send_message(id, "bye").unwrap(); // let it end cleanly
    engine.join();
}

#[test]
fn persistent_agent_survives_per_turn_results() {
    let mut engine = Engine::new();
    // persistent = true: a per-turn `result` must NOT terminalize the agent.
    let id = engine
        .spawn(script_spec("mock-convo.py"), "convo", Tags::empty(), None, false, true)
        .unwrap();

    let tail_has = |e: &Engine, needle: &str| {
        e.registry()
            .record(id)
            .map(|r| r.tail.iter().any(|l| l.text.contains(needle)))
            .unwrap_or(false)
    };

    engine.send_message(id, "one").unwrap();
    assert!(pump_until(&mut engine, |e| tail_has(e, "echo: one")));
    // Turn 1's `result` left the agent alive (not Done).
    assert!(!engine.registry().record(id).unwrap().state.is_terminal());

    // The follow-up reaches the STILL-LIVE agent — the thing that was broken.
    engine.send_message(id, "two").unwrap();
    assert!(pump_until(&mut engine, |e| tail_has(e, "echo: two")));
    assert!(!engine.registry().record(id).unwrap().state.is_terminal());

    engine.shutdown(); // kills it; only now does it terminate
    engine.join();
}

#[test]
fn streamed_reply_finalizes_to_single_line() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(script_spec("mock-stream.py"), "s", Tags::empty(), None, false, false)
        .unwrap();

    assert!(pump_until(&mut engine, |e| e.registry().all_terminal()));

    let rec = engine.registry().record(id).unwrap();
    let text_lines: Vec<&str> = rec
        .tail
        .iter()
        .filter(|l| l.kind == LineKind::Text)
        .map(|l| l.text.as_str())
        .collect();
    // Streamed then finalized: exactly one Text line with the whole reply, not
    // one per chunk and not doubled by the final complete message.
    assert_eq!(text_lines, vec!["Hello world, streamed live!"]);

    engine.join();
}

#[test]
fn work_agent_renders_tool_call_result_and_markdown() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(script_spec("mock-work.py"), "w", Tags::empty(), None, false, false)
        .unwrap();

    assert!(pump_until(&mut engine, |e| e.registry().all_terminal()));

    let rec = engine.registry().record(id).unwrap();
    let has = |kind: LineKind, needle: &str| {
        rec.tail.iter().any(|l| l.kind == kind && l.text.contains(needle))
    };
    // Content that used to be dropped now reaches the window, Claude-style.
    assert!(has(LineKind::ToolCall, "Bash(ls -la)"), "tool call with args");
    assert!(has(LineKind::ToolResult, "Cargo.toml"), "tool output shown");
    assert!(has(LineKind::Text, "Summary"), "markdown answer shown");

    engine.join();
}

#[test]
fn killing_a_blocked_agent_makes_it_terminal() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(mock_spec(), "victim", Tags::empty(), None, false, false)
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
    let id = engine.spawn(mock_spec(), "d", Tags::empty(), None, false, false).unwrap();

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

/// Esc → interrupt: a running turn is stopped without killing the session. The
/// interrupt control_request reaches the process (it acks with "turn-interrupted")
/// and the pane shows the `⎋ interrupted` note; the persistent session stays alive.
#[test]
fn interrupt_stops_the_turn_and_keeps_the_session_alive() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(script_spec("mock-interrupt.py"), "int", Tags::empty(), None, false, true)
        .unwrap();

    let tail_has = |e: &Engine, needle: &str| {
        e.registry()
            .record(id)
            .map(|r| r.tail.iter().any(|l| l.text.contains(needle)))
            .unwrap_or(false)
    };

    // A running turn (a tool call streamed in, no gate).
    assert!(pump_until(&mut engine, |e| e
        .registry()
        .record(id)
        .map(|r| matches!(r.state, AgentState::Working))
        .unwrap_or(false)));
    assert!(
        engine.registry().pending_request_id(id).is_none(),
        "this is a running turn, not an approval gate"
    );

    engine.interrupt(id).unwrap();

    // The interrupt reached the process (it acked) and the note is shown.
    assert!(pump_until(&mut engine, |e| tail_has(e, "turn-interrupted")));
    assert!(tail_has(&engine, "\u{238b}"), "the ⎋ interrupted note is shown");
    // Session survives — persistent process, not terminal.
    assert!(!engine.registry().record(id).unwrap().state.is_terminal());

    engine.shutdown();
    engine.join();
}

/// Interrupt is a no-op unless the agent is actively working — a BlockedOnApproval
/// agent is answered (y/n), never interrupted; its gate must stay untouched.
#[test]
fn interrupt_is_a_noop_for_a_blocked_agent() {
    let mut engine = Engine::new();
    let id = engine.spawn(mock_spec(), "b", Tags::empty(), None, false, false).unwrap();

    assert!(pump_until(&mut engine, |e| e
        .registry()
        .pending_request_id(id)
        .is_some()));
    let before = engine.registry().record(id).unwrap().tail.len();

    engine.interrupt(id).unwrap(); // blocked, not Working → no-op

    assert_eq!(
        engine.registry().record(id).unwrap().tail.len(),
        before,
        "interrupt must add no note to a blocked agent"
    );
    assert!(
        engine.registry().pending_request_id(id).is_some(),
        "the gate must remain pending"
    );

    engine.answer(id, Decision::Allow).unwrap(); // let it finish cleanly
    engine.join();
}

/// A message typed while an agent is blocked on approval is QUEUED (the process is
/// mid-can_use_tool, waiting for a control_response) and flushed as a real user
/// turn the moment the gate resolves — the agent then echoes it.
#[test]
fn message_typed_while_blocked_is_delivered_after_gate_resolves() {
    let mut engine = Engine::new();
    let id = engine
        .spawn(script_spec("mock-gate-chat.py"), "gc", Tags::empty(), None, false, true)
        .unwrap();

    assert!(pump_until(&mut engine, |e| e
        .registry()
        .pending_request_id(id)
        .is_some()));

    // Message the blocked agent: queued, not sent yet.
    engine.send_message(id, "while blocked").unwrap();
    {
        let rec = engine.registry().record(id).unwrap();
        assert_eq!(rec.state, AgentState::BlockedOnApproval);
        assert!(
            rec.tail.iter().any(|l| l.text.contains("you: while blocked")),
            "the user's line is echoed immediately"
        );
        assert!(rec.pending_message.is_some(), "message is held until the gate resolves");
    }

    // Approve → gate resolves → queued message flushes → mock echoes it back.
    engine.answer(id, Decision::Allow).unwrap();
    assert!(pump_until(&mut engine, |e| e
        .registry()
        .record(id)
        .map(|r| r.tail.iter().any(|l| l.text.contains("echo: while blocked")))
        .unwrap_or(false)));
    assert!(
        engine.registry().record(id).unwrap().pending_message.is_none(),
        "the queue is cleared once delivered"
    );

    engine.shutdown();
    engine.join();
}

/// Real-shape sub-agent approval routing: a parent that spawns two background
/// sub-agents (via the `Agent` tool) whose inner Bash calls each raise a
/// `can_use_tool` gate. Each gate must land in its OWN sub-agent pane (correlated
/// by the gate's `tool_use_id`), never colliding on the root. Mirrors the live
/// claude capture frozen in `fixtures/subagent-approval.jsonl`.
#[test]
fn subagent_gates_route_to_child_panes_not_root() {
    let mut engine = Engine::new();
    // Persistent: the mock ends its turn (result → TurnEnded) while the sub-agents
    // stay blocked; the child panes must survive that.
    let root = engine
        .spawn(script_spec("mock-subagents.py"), "root", Tags::empty(), None, false, true)
        .unwrap();

    // Both sub-agent panes appear and each blocks on its OWN gate.
    let ready = pump_until(&mut engine, |e| {
        let subs: Vec<AgentId> = e
            .registry()
            .order()
            .iter()
            .copied()
            .filter(|id| *id != root)
            .collect();
        subs.len() == 2
            && subs
                .iter()
                .all(|id| e.registry().pending_request_id(*id).is_some())
    });
    assert!(ready, "both sub-agent panes should block on their own gate");

    // The root (parent) is NOT the one holding a gate.
    assert!(
        engine.registry().pending_request_id(root).is_none(),
        "gates must not collide on the root pane"
    );

    let subs: Vec<AgentId> = engine
        .registry()
        .order()
        .iter()
        .copied()
        .filter(|id| *id != root)
        .collect();
    let r0 = engine.registry().pending_request_id(subs[0]).unwrap();
    let r1 = engine.registry().pending_request_id(subs[1]).unwrap();
    assert_ne!(r0, r1, "each sub-agent pane holds a distinct request_id");

    // Answering one child clears only its block (the answer is routed to the root
    // process's control channel with the child's own request_id).
    engine.answer(subs[0], Decision::Allow).unwrap();
    assert!(engine.registry().pending_request_id(subs[0]).is_none());
    assert!(
        engine.registry().pending_request_id(subs[1]).is_some(),
        "answering one sub-agent must not resolve the other"
    );

    engine.shutdown();
    engine.join();
}

/// Session restore + LIVE re-attach: a persisted pane (terminal, with saved
/// transcript) is restored, then a fresh process is attached to the SAME agent
/// id via `resume_agent`. The pane must reactivate (stop being terminal) and the
/// resumed process's output must APPEND to the preserved transcript. This mirrors
/// `claude --resume` with a mock standing in for claude (tests never run claude).
#[test]
fn restore_then_resume_attaches_live_process_to_existing_pane() {
    let id = AgentId(7);
    let state = SessionState {
        version: 1,
        projects: vec![Project {
            id: ProjectId(0),
            name: "proj".into(),
            cwd: "/tmp".into(),
        }],
        active: ProjectId(0),
        agents: vec![AgentSnapshot {
            project_id: ProjectId(0),
            meta: AgentMeta::new(id, "revived", "/tmp".into(), 0),
            state: AgentState::Done, // terminal — reactivate must lift this
            info: None,
            tokens: TokenUsage::default(),
            tail: vec![TranscriptLine::new(LineKind::Text, "OLD HISTORY LINE")],
            session_id: Some("s-mock".into()),
            is_subagent: false,
            resumable: true,
            kind: awm_core::session::PaneKind::Agent,
        }],
    };

    let mut engine = Engine::new();
    engine.registry_mut().restore(&state);

    // Restored as dead history: terminal, not live, old line present.
    assert_eq!(engine.registry().record(id).unwrap().state, AgentState::Done);
    assert!(!engine.is_live(id));

    // Re-attach a live (mock) process to the SAME pane.
    engine
        .resume_agent(id, mock_spec(), None, false, false)
        .unwrap();
    assert!(engine.is_live(id), "a live process is now attached");

    // The resumed process's output appends to the pane.
    let grew = pump_until(&mut engine, |e| {
        e.registry()
            .record(id)
            .map(|r| r.tail.iter().any(|l| l.text.contains("session started")))
            .unwrap_or(false)
    });
    assert!(grew, "resumed output should append to the restored pane");

    let rec = engine.registry().record(id).unwrap();
    assert!(
        rec.tail.iter().any(|l| l.text.contains("OLD HISTORY LINE")),
        "the restored transcript is preserved across resume"
    );
    assert_ne!(rec.state, AgentState::Done, "pane reactivated, no longer terminal");

    engine.shutdown();
    engine.join();
}
