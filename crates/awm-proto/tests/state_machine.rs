//! Contract tests for the frozen state machine. These are GREEN and gate any
//! future change to `awm-proto`.

use awm_proto::{AgentEvent, AgentState, ApprovalCtx, TokenUsage};
use proptest::prelude::*;

fn approval() -> AgentEvent {
    AgentEvent::ApprovalRequested(ApprovalCtx {
        tool: "Bash".into(),
        input: serde_json::json!({ "command": "ls" }),
        request_id: "req-1".into(),
        tool_use_id: Some("toolu_1".into()),
        description: Some("ls".into()),
        decision_reason: None,
        diff: None,
    })
}

// --- The canonical happy path: Idle → Working → Blocked → Working → Done ---

#[test]
fn full_lifecycle_through_approval() {
    let mut s = AgentState::Idle;
    s = s.apply(&AgentEvent::Started {
        model: "opus".into(),
        cwd: "/tmp".into(),
    });
    assert_eq!(s, AgentState::Working);

    s = s.apply(&approval());
    assert_eq!(s, AgentState::BlockedOnApproval);
    assert!(s.is_blocked());

    s = s.apply(&AgentEvent::ApprovalResolved { approved: true });
    assert_eq!(s, AgentState::Working);

    s = s.apply(&AgentEvent::Finished { ok: true });
    assert_eq!(s, AgentState::Done);
    assert!(s.is_terminal());
}

#[test]
fn denied_approval_resumes_working() {
    let s = AgentState::BlockedOnApproval.apply(&AgentEvent::ApprovalResolved { approved: false });
    assert_eq!(s, AgentState::Working);
}

#[test]
fn turn_ended_returns_to_idle_not_terminal() {
    // A persistent agent's per-turn completion is not the end of the session.
    let s = AgentState::Working.apply(&AgentEvent::TurnEnded { ok: true });
    assert_eq!(s, AgentState::Idle);
    assert!(!s.is_terminal());
}

#[test]
fn failure_is_terminal() {
    let s = AgentState::Working.apply(&AgentEvent::Finished { ok: false });
    assert_eq!(s, AgentState::Failed);
    assert!(s.is_terminal());
}

// --- Inertness / robustness ---

#[test]
fn tokens_and_noise_never_transition() {
    for start in [
        AgentState::Idle,
        AgentState::Working,
        AgentState::BlockedOnApproval,
    ] {
        assert_eq!(start.apply(&AgentEvent::Noise), start);
        assert_eq!(
            start.apply(&AgentEvent::Tokens(TokenUsage {
                input: 10,
                output: 5
            })),
            start
        );
    }
}

#[test]
fn terminal_states_absorb_everything() {
    let events = [
        AgentEvent::Started {
            model: "m".into(),
            cwd: "/".into(),
        },
        AgentEvent::Thinking { text: String::new() },
        AgentEvent::ToolStarted { name: "Edit".into(), summary: "f.txt".into() },
        approval(),
        AgentEvent::ApprovalResolved { approved: true },
        AgentEvent::Finished { ok: true },
        AgentEvent::Finished { ok: false },
        AgentEvent::Noise,
    ];
    for terminal in [AgentState::Done, AgentState::Failed] {
        for ev in &events {
            assert_eq!(terminal.apply(ev), terminal, "terminal must absorb {ev:?}");
        }
    }
}

#[test]
fn thinking_and_tool_keep_agent_working() {
    assert_eq!(
        AgentState::Idle.apply(&AgentEvent::Thinking { text: String::new() }),
        AgentState::Working
    );
    assert_eq!(
        AgentState::Working.apply(&AgentEvent::ToolStarted { name: "Read".into(), summary: "f.txt".into() }),
        AgentState::Working
    );
}

// --- Serde stability: the wire shape is part of the contract ---

#[test]
fn event_and_state_serde_round_trip() {
    let ev = approval();
    let json = serde_json::to_string(&ev).unwrap();
    assert!(json.contains("\"kind\":\"approval_requested\""), "{json}");
    assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);

    let st = AgentState::BlockedOnApproval;
    assert_eq!(serde_json::to_string(&st).unwrap(), "\"blocked_on_approval\"");
    assert_eq!(
        serde_json::from_str::<AgentState>("\"done\"").unwrap(),
        AgentState::Done
    );
}

// --- Property: totality. No event sequence ever panics or escapes terminality. ---

fn arb_event() -> impl Strategy<Value = AgentEvent> {
    prop_oneof![
        Just(AgentEvent::Thinking { text: String::new() }),
        Just(AgentEvent::Noise),
        Just(AgentEvent::ToolStarted { name: "T".into(), summary: "s".into() }),
        Just(approval()),
        any::<bool>().prop_map(|approved| AgentEvent::ApprovalResolved { approved }),
        any::<bool>().prop_map(|ok| AgentEvent::Finished { ok }),
        Just(AgentEvent::Started {
            model: "m".into(),
            cwd: "/".into()
        }),
    ]
}

proptest! {
    #[test]
    fn totality_and_terminal_stickiness(events in prop::collection::vec(arb_event(), 0..64)) {
        let mut s = AgentState::Idle;
        let mut seen_terminal = false;
        for ev in &events {
            let next = s.apply(ev); // must never panic
            if seen_terminal {
                prop_assert_eq!(next, s, "terminal state changed");
            }
            if next.is_terminal() {
                seen_terminal = true;
            }
            s = next;
        }
    }
}
