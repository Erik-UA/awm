//! Track B acceptance test (RED until Phase 2). Drives every fixture through the
//! parser and the frozen state machine, asserting the observable outcome recorded
//! in each `*.expected.json`. Fails at `StreamParser::feed` (`unimplemented!`)
//! until Track B lands.

use awm_parser::StreamParser;
use awm_proto::{AgentEvent, AgentState, EventSource};
use std::fs;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn state_name(s: AgentState) -> String {
    serde_json::to_value(s)
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
}

fn run_fixture(name: &str) {
    let dir = fixtures_dir();
    let bytes = fs::read(dir.join(format!("{name}.jsonl"))).unwrap();
    let expected: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join(format!("{name}.expected.json"))).unwrap())
            .unwrap();

    let mut parser = StreamParser::new();
    parser.feed(&bytes);

    let mut state = AgentState::Idle;
    let mut states: Vec<String> = Vec::new();
    let mut approvals: Vec<(String, String)> = Vec::new();
    let mut last_tokens: Option<(u64, u64)> = None;

    while let Some(ev) = parser.next_event() {
        match &ev {
            AgentEvent::ApprovalRequested(ctx) => {
                approvals.push((ctx.tool.clone(), ctx.request_id.clone()));
            }
            AgentEvent::Tokens(t) => last_tokens = Some((t.input, t.output)),
            _ => {}
        }
        let next = state.apply(&ev);
        if next != state {
            states.push(state_name(next)); // collapse consecutive duplicates
        }
        state = next;
    }

    // --- States trajectory ---
    let want_states: Vec<String> = expected["states"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(states, want_states, "[{name}] state trajectory");

    // --- Final state ---
    assert_eq!(
        state_name(state),
        expected["final_state"].as_str().unwrap(),
        "[{name}] final state"
    );

    // --- Approvals captured ---
    let want_approvals: Vec<(String, String)> = expected["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["tool"].as_str().unwrap().to_string(),
                a["request_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(approvals, want_approvals, "[{name}] approvals");

    // --- Final token accounting ---
    let ft = &expected["final_tokens"];
    assert_eq!(
        last_tokens,
        Some((ft["input"].as_u64().unwrap(), ft["output"].as_u64().unwrap())),
        "[{name}] final tokens"
    );
}

#[test]
fn normal() {
    run_fixture("normal");
}

#[test]
fn approval() {
    run_fixture("approval");
}

#[test]
fn error() {
    run_fixture("error");
}

#[test]
fn subagents() {
    run_fixture("subagents");
}

#[test]
fn subagent_approval() {
    run_fixture("subagent-approval");
}

#[test]
fn garbage_is_robust() {
    run_fixture("garbage");
}

/// The `init` line's `session_id` is surfaced on `AgentInfo` — the key the
/// runtime later uses to resume a persisted session (`claude --resume <id>`).
#[test]
fn session_id_is_extracted_from_init() {
    let bytes = fs::read(fixtures_dir().join("normal.jsonl")).unwrap();
    let mut parser = StreamParser::new();
    parser.feed(&bytes);

    let mut session_id = None;
    while let Some(ev) = parser.next_event() {
        if let AgentEvent::Info(info) = ev {
            session_id = info.session_id.clone();
        }
    }
    assert_eq!(session_id.as_deref(), Some("s-normal"), "session_id from init");
}
