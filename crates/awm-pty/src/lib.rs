//! Track A — PTY session management + agent-runner control channel.
//!
//! Two things live here:
//! 1. [`PtySession`] — spawn/kill/resize a child inside a PTY with a bounded
//!    output ring buffer (the manual `e`/attach case).
//! 2. [`StreamJsonRunner`] — spawn the agent over piped stdio in stream-json
//!    mode, expose raw stdout bytes to the parser (Track B), and act as the
//!    permission controller by writing `control_response` lines on a
//!    `can_use_tool` gate (see `docs/approval-findings.md`).

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Wrap any `Display`able error (e.g. `portable-pty`'s `anyhow::Error`) as an
/// `io::Error` so the public API can stay `std::io::Result`.
fn other_err<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}

/// Minimal JSON string escaper for the small set of fields we serialize by hand
/// (`request_id`, deny messages, prompt text). Avoids pulling in a JSON crate
/// just to emit a couple of fixed-shape lines.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

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
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    ring: Arc<Mutex<VecDeque<String>>>,
    reader: Option<JoinHandle<()>>,
}

impl PtySession {
    /// Spawn `spec` in a new PTY. Retains the last `ring_lines` lines of output.
    pub fn spawn(spec: &CommandSpec, ring_lines: usize) -> std::io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(other_err)?;

        let mut cmd = CommandBuilder::new(&spec.program);
        for a in &spec.args {
            cmd.arg(a);
        }
        cmd.cwd(&spec.cwd);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd).map_err(other_err)?;
        // Drop the slave so the reader sees EOF once the child exits and its
        // own slave fd is the last one closed.
        drop(pair.slave);

        let reader_handle = pair.master.try_clone_reader().map_err(other_err)?;
        let ring: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let ring_for_thread = Arc::clone(&ring);
        let reader = std::thread::spawn(move || {
            pump_lines(reader_handle, ring_for_thread, ring_lines);
        });

        Ok(PtySession {
            master: pair.master,
            child,
            ring,
            reader: Some(reader),
        })
    }

    /// The most recent `n` buffered output lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let guard = self.ring.lock().unwrap();
        let start = guard.len().saturating_sub(n);
        guard.iter().skip(start).cloned().collect()
    }

    /// Resize the PTY window.
    pub fn resize(&mut self, rows: u16, cols: u16) -> std::io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(other_err)
    }

    /// Block until the child exits and return its status code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        let status = self.child.wait().map_err(other_err)?;
        // Drain the reader thread so the ring buffer reflects all output before
        // callers `tail()`.
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        Ok(status.exit_code() as i32)
    }

    /// Terminate the child.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().map_err(other_err)
    }
}

/// Read `reader` to EOF, splitting into lines and pushing them into `ring`,
/// evicting from the front so at most `cap` lines are retained.
fn pump_lines(mut reader: Box<dyn Read + Send>, ring: Arc<Mutex<VecDeque<String>>>, cap: usize) {
    let mut buf = [0u8; 4096];
    let mut line: Vec<u8> = Vec::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\n' {
                        push_line(&ring, &mut line, cap);
                    } else {
                        line.push(b);
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if !line.is_empty() {
        push_line(&ring, &mut line, cap);
    }
}

fn push_line(ring: &Arc<Mutex<VecDeque<String>>>, line: &mut Vec<u8>, cap: usize) {
    // PTY output is CRLF-terminated; drop the trailing CR.
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let s = String::from_utf8_lossy(line).into_owned();
    line.clear();
    let mut guard = ring.lock().unwrap();
    guard.push_back(s);
    while guard.len() > cap {
        guard.pop_front();
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
/// The public API is synchronous; a small current-thread tokio runtime drives
/// the async child process underneath.
pub struct StreamJsonRunner {
    rt: Arc<tokio::runtime::Runtime>,
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    answerer: Answerer,
}

impl StreamJsonRunner {
    /// Spawn the agent with piped stdin/stdout in stream-json mode.
    pub fn spawn(spec: &CommandSpec) -> std::io::Result<Self> {
        // A multi-thread runtime (one worker) so the reader thread and a
        // separately-held [`Answerer`] can both `block_on` concurrently — the
        // control-channel write must not deadlock behind a blocking `read`.
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?,
        );

        let (child, stdin, stdout) = rt.block_on(async {
            let mut cmd = tokio::process::Command::new(&spec.program);
            cmd.args(&spec.args)
                .current_dir(&spec.cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }
            let mut child = cmd.spawn()?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| other_err("child stdin was not piped"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| other_err("child stdout was not piped"))?;
            Ok::<_, std::io::Error>((child, stdin, stdout))
        })?;

        let answerer = Answerer {
            rt: rt.clone(),
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
        };

        Ok(StreamJsonRunner {
            rt,
            child,
            stdout,
            answerer,
        })
    }

    /// A cheap `Send + Sync` handle to this agent's stdin, for answering
    /// approvals / sending prompts from another thread while `read` blocks here.
    pub fn answerer(&self) -> Answerer {
        self.answerer.clone()
    }

    /// Send a user prompt as a stream-json input message.
    pub fn send_prompt(&mut self, text: &str) -> std::io::Result<()> {
        self.answerer.send_prompt(text)
    }

    /// Perform the SDK control-protocol `initialize` handshake. Required before a
    /// real `claude` (run with `--permission-prompt-tool stdio`) will route
    /// `can_use_tool` approval gates to us. Not used for mock agents.
    pub fn send_initialize(&mut self) -> std::io::Result<()> {
        self.answerer.send_initialize()
    }

    /// Read the next chunk of raw stdout bytes to feed the parser. Returns an
    /// empty vec at end-of-stream.
    pub fn read(&mut self) -> std::io::Result<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        let StreamJsonRunner { rt, stdout, .. } = self;
        rt.block_on(async {
            let mut buf = vec![0u8; 8192];
            let n = stdout.read(&mut buf).await?;
            buf.truncate(n);
            Ok(buf)
        })
    }

    /// Answer a pending `can_use_tool` request (by envelope `request_id`) by
    /// writing the corresponding `control_response` line.
    pub fn answer(&mut self, request_id: &str, decision: Decision) -> std::io::Result<()> {
        self.answerer.answer(request_id, decision)
    }

    /// Block until the agent exits and return its status code.
    pub fn wait(&mut self) -> std::io::Result<i32> {
        let StreamJsonRunner { rt, child, .. } = self;
        let status = rt.block_on(async { child.wait().await })?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// A `Send + Sync` handle to an agent's stdin. Held by the UI thread so it can
/// answer approval gates and send prompts while the runner's `read` loop blocks
/// on a reader thread. Cheap to clone (shares the runtime and stdin).
#[derive(Clone)]
pub struct Answerer {
    rt: Arc<tokio::runtime::Runtime>,
    stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
}

impl Answerer {
    /// Answer a pending `can_use_tool` request by its envelope `request_id`.
    pub fn answer(&self, request_id: &str, decision: Decision) -> std::io::Result<()> {
        let inner = match decision {
            Decision::Allow => r#"{"behavior":"allow","updatedInput":{}}"#.to_string(),
            Decision::Deny(reason) => {
                format!(r#"{{"behavior":"deny","message":"{}"}}"#, json_escape(&reason))
            }
        };
        let line = format!(
            r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"{}","response":{}}}}}"#,
            json_escape(request_id),
            inner
        );
        self.write_line(&line)
    }

    /// Send a user prompt as a stream-json input message.
    pub fn send_prompt(&self, text: &str) -> std::io::Result<()> {
        let line = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{}"}}}}"#,
            json_escape(text)
        );
        self.write_line(&line)
    }

    /// Send the `initialize` control_request that opens the SDK control protocol.
    pub fn send_initialize(&self) -> std::io::Result<()> {
        self.write_line(
            r#"{"type":"control_request","request_id":"awm-init","request":{"subtype":"initialize"}}"#,
        )
    }

    /// Write a single newline-terminated line to the agent's stdin and flush.
    fn write_line(&self, line: &str) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        let stdin = self.stdin.clone();
        self.rt.block_on(async move {
            let mut guard = stdin.lock().await;
            guard.write_all(&bytes).await?;
            guard.flush().await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pushing more than `cap` lines evicts the oldest, keeping the newest `cap`.
    #[test]
    fn ring_buffer_evicts_oldest_beyond_cap() {
        let ring: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let cap = 3;
        for i in 0..10u32 {
            // Include a trailing CR to confirm it is stripped.
            let mut line = format!("line{i}\r").into_bytes();
            push_line(&ring, &mut line, cap);
        }
        let guard = ring.lock().unwrap();
        let got: Vec<&str> = guard.iter().map(String::as_str).collect();
        assert_eq!(got, vec!["line7", "line8", "line9"]);
        assert_eq!(guard.len(), cap);
    }

    fn mock_agent() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mock-agent.py")
    }

    fn extract_request_id(buf: &str) -> Option<String> {
        for line in buf.lines() {
            if line.contains("\"can_use_tool\"") {
                let anchor = "\"request_id\"";
                let i = line.find(anchor)?;
                let rest = line[i + anchor.len()..].trim_start();
                let rest = rest.strip_prefix(':')?.trim_start();
                let rest = rest.strip_prefix('"')?;
                let j = rest.find('"')?;
                return Some(rest[..j].to_string());
            }
        }
        None
    }

    /// A `Decision::Deny` answer makes the mock agent error out and exit 1.
    #[test]
    fn deny_decision_makes_mock_exit_nonzero() {
        let spec =
            CommandSpec::new("python3", std::env::temp_dir()).arg(mock_agent().to_str().unwrap());
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
                if let Some(rid) = extract_request_id(&buf) {
                    runner
                        .answer(&rid, Decision::Deny("not allowed in test".into()))
                        .unwrap();
                    answered = true;
                }
            }
        }

        let code = runner.wait().unwrap();
        assert!(answered, "should have observed a can_use_tool request");
        assert_eq!(code, 1, "mock must exit 1 after deny");
        assert!(buf.contains("denied"), "expected a denial result; stream: {buf}");
        assert!(
            !buf.contains("post-approval-tool-result"),
            "agent must not proceed after deny; stream: {buf}"
        );
    }
}
