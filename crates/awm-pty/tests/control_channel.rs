//! Track A acceptance test for the agent-runner control channel (RED until
//! Phase 2). Drives a deterministic **mock agent** (never live `claude`) that
//! raises a `can_use_tool` gate and blocks until answered, proving the runner's
//! approve/deny write-path. Fails at `StreamJsonRunner::spawn` (`unimplemented!`)
//! until Track A lands.

use awm_pty::{CommandSpec, Decision, StreamJsonRunner};
use std::path::PathBuf;

fn mock_agent() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mock-agent.py")
}

/// Extract the envelope `request_id` of a `can_use_tool` control_request line.
fn find_request_id(buf: &str) -> Option<String> {
    for line in buf.lines() {
        if line.contains("\"can_use_tool\"") {
            let key = "\"request_id\":\"";
            if let Some(i) = line.find(key) {
                let rest = &line[i + key.len()..];
                if let Some(j) = rest.find('"') {
                    return Some(rest[..j].to_string());
                }
            }
        }
    }
    None
}

#[test]
fn answers_approval_and_agent_proceeds() {
    let spec = CommandSpec::new("python3", std::env::temp_dir())
        .arg(mock_agent().to_str().unwrap());

    let mut runner = StreamJsonRunner::spawn(&spec).unwrap();

    let mut buf = String::new();
    let mut answered = false;
    loop {
        let chunk = runner.read().unwrap();
        if chunk.is_empty() {
            break; // EOF
        }
        buf.push_str(&String::from_utf8_lossy(&chunk));

        if !answered {
            if let Some(rid) = find_request_id(&buf) {
                runner.answer(&rid, Decision::Allow).unwrap();
                answered = true;
            }
        }
        if buf.contains("\"type\": \"result\"") || buf.contains("\"type\":\"result\"") {
            break;
        }
    }

    let code = runner.wait().unwrap();
    assert_eq!(code, 0, "agent should exit 0 after approval");
    assert!(answered, "runner should have observed a can_use_tool request");
    assert!(
        buf.contains("post-approval-tool-result"),
        "agent must proceed after allow; stream was: {buf}"
    );
}
