//! Track A — PTY session management. **Stub for Phase 2.**
//!
//! The public API below is the frozen surface the Track A subagent implements
//! against; bodies are `unimplemented!()` so acceptance tests compile and fail
//! (the red gate) until the work lands.

#![forbid(unsafe_code)]

use std::path::PathBuf;

/// How to launch a child process inside a PTY.
#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    /// A bare command in a directory, no extra args or env.
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        CommandSpec {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
}

/// A running (or finished) PTY-backed child with a bounded output ring buffer.
pub struct PtySession {
    _private: (),
}

impl PtySession {
    /// Spawn `spec` in a new PTY. Retains the last `ring_lines` lines of output.
    pub fn spawn(_spec: &CommandSpec, _ring_lines: usize) -> std::io::Result<Self> {
        unimplemented!("Track A / Phase 2")
    }

    /// The most recent `n` buffered output lines, oldest first.
    pub fn tail(&self, _n: usize) -> Vec<String> {
        unimplemented!("Track A / Phase 2")
    }

    /// Resize the PTY window.
    pub fn resize(&mut self, _rows: u16, _cols: u16) -> std::io::Result<()> {
        unimplemented!("Track A / Phase 2")
    }

    /// Block until the child exits and return its status code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        unimplemented!("Track A / Phase 2")
    }

    /// Terminate the child.
    pub fn kill(&mut self) -> std::io::Result<()> {
        unimplemented!("Track A / Phase 2")
    }
}

/// A permission decision for a pending `can_use_tool` control_request.
#[derive(Clone, Debug)]
pub enum Decision {
    /// Approve; optionally pass through updated input (usually the original).
    Allow,
    /// Reject with a human-readable reason.
    Deny(String),
}

/// Runs an agent (e.g. `claude -p --input-format stream-json
/// --output-format stream-json`) over **piped stdio** and acts as the permission
/// controller. Raw stdout bytes are exposed for the parser (Track B); approval
/// gates are answered by writing a `control_response` line back to the agent.
///
/// This is the primary integration path for the killer feature — see
/// `docs/approval-findings.md`. **Stub for Phase 2.**
pub struct StreamJsonRunner {
    _private: (),
}

impl StreamJsonRunner {
    /// Spawn the agent with piped stdin/stdout in stream-json mode.
    pub fn spawn(_spec: &CommandSpec) -> std::io::Result<Self> {
        unimplemented!("Track A / Phase 2")
    }

    /// Send a user prompt as a stream-json input message.
    pub fn send_prompt(&mut self, _text: &str) -> std::io::Result<()> {
        unimplemented!("Track A / Phase 2")
    }

    /// Read the next chunk of raw stdout bytes to feed the parser. Returns an
    /// empty vec at end-of-stream.
    pub fn read(&mut self) -> std::io::Result<Vec<u8>> {
        unimplemented!("Track A / Phase 2")
    }

    /// Answer a pending `can_use_tool` request (by envelope `request_id`) by
    /// writing the corresponding `control_response` line.
    pub fn answer(&mut self, _request_id: &str, _decision: Decision) -> std::io::Result<()> {
        unimplemented!("Track A / Phase 2")
    }

    /// Block until the agent exits and return its status code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        unimplemented!("Track A / Phase 2")
    }
}
