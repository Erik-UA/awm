//! Track A acceptance test (RED until Phase 2). Spawns a trivial command in a
//! PTY, reads its output from the ring buffer, and checks the exit status.
//! Fails at `PtySession::spawn` (`unimplemented!`) until Track A lands.

use awm_pty::{CommandSpec, PtySession};

#[test]
fn spawn_echo_reads_output_and_exits_zero() {
    let spec = CommandSpec::new("bash", std::env::temp_dir())
        .arg("-c")
        .arg("echo hi");

    let mut session = PtySession::spawn(&spec, 100).unwrap();

    let code = session.wait().unwrap();
    assert_eq!(code, 0, "echo should exit 0");

    let out = session.tail(10).join("\n");
    assert!(out.contains("hi"), "expected 'hi' in output, got: {out:?}");
}
