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
