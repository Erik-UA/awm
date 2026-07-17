//! The runtime engine — spawns agents, streams their events into the registry,
//! and answers approvals over the control channel.
//!
//! Each agent gets a reader thread that blocks on [`StreamJsonRunner::read`],
//! parses bytes into [`AgentEvent`]s, and forwards them over an mpsc channel.
//! The owning (UI) thread holds an [`Answerer`] per agent so it can approve/deny
//! a gate while the reader thread is blocked — no deadlock.

use crate::registry::Registry;
use awm_parser::StreamParser;
use awm_proto::{AgentEvent, AgentId, AgentMeta, EventSource, Tags};
use awm_pty::{Answerer, CommandSpec, Decision, StreamJsonRunner};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

/// An event tagged with the agent it came from.
pub struct CoreEvent {
    pub id: AgentId,
    pub event: AgentEvent,
}

/// Owns the model and every agent's I/O plumbing.
pub struct Engine {
    reg: Registry,
    tx: Sender<CoreEvent>,
    rx: Receiver<CoreEvent>,
    answerers: HashMap<AgentId, Answerer>,
    pids: HashMap<AgentId, u32>,
    readers: Vec<JoinHandle<()>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Engine {
            reg: Registry::new(),
            tx,
            rx,
            answerers: HashMap::new(),
            pids: HashMap::new(),
            readers: Vec::new(),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.reg
    }

    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.reg
    }

    /// Spawn an agent from `spec`, optionally sending an initial `prompt`.
    /// Starts its reader thread and returns the new agent id.
    ///
    /// `handshake` runs the SDK `initialize` control-protocol handshake before
    /// the prompt — required for a real `claude` (with `--permission-prompt-tool
    /// stdio`) to route approval gates to us; leave it `false` for mock agents.
    pub fn spawn(
        &mut self,
        spec: CommandSpec,
        name: impl Into<String>,
        tags: Tags,
        prompt: Option<String>,
        handshake: bool,
    ) -> std::io::Result<AgentId> {
        let id = self.reg.alloc_id();
        let mut meta = AgentMeta::new(id, name, spec.cwd.clone(), 0);
        meta.tags = tags;
        self.reg.add(meta);

        let mut runner = StreamJsonRunner::spawn(&spec)?;
        if handshake {
            runner.send_initialize()?;
        }
        if let Some(p) = prompt {
            runner.send_prompt(&p)?;
        }
        self.answerers.insert(id, runner.answerer());
        if let Some(pid) = runner.pid() {
            self.pids.insert(id, pid);
        }

        let tx = self.tx.clone();
        let handle = std::thread::spawn(move || {
            let mut parser = StreamParser::new();
            loop {
                match runner.read() {
                    Ok(chunk) if chunk.is_empty() => break, // EOF
                    Ok(chunk) => {
                        parser.feed(&chunk);
                        while let Some(event) = parser.next_event() {
                            if tx.send(CoreEvent { id, event }).is_err() {
                                return; // engine dropped
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            // Process-death safety net: if the stream ended without a `result`
            // (the child crashed or was killed), this marks the agent Failed.
            // If it already finished cleanly, the terminal state absorbs it.
            let _ = tx.send(CoreEvent {
                id,
                event: AgentEvent::Finished { ok: false },
            });
            let _ = runner.wait();
        });
        self.readers.push(handle);
        Ok(id)
    }

    /// Drain all currently-ready events into the registry. Returns how many.
    pub fn pump(&mut self) -> usize {
        let mut n = 0;
        while let Ok(ce) = self.rx.try_recv() {
            self.reg.apply_event(ce.id, &ce.event);
            n += 1;
        }
        n
    }

    /// Wait up to `timeout` for at least one event, then drain the rest.
    pub fn pump_blocking(&mut self, timeout: Duration) -> usize {
        match self.rx.recv_timeout(timeout) {
            Ok(ce) => {
                self.reg.apply_event(ce.id, &ce.event);
                1 + self.pump()
            }
            Err(_) => 0,
        }
    }

    /// Answer the pending approval for `id`. Writes the `control_response` on the
    /// agent's stdin and — because we generate that response ourselves, so it
    /// never comes back on stdout — synthesizes the matching `ApprovalResolved`
    /// into the registry to unblock the agent.
    pub fn answer(&mut self, id: AgentId, decision: Decision) -> std::io::Result<()> {
        let Some(request_id) = self.reg.pending_request_id(id) else {
            return Ok(()); // nothing pending
        };
        let approved = matches!(decision, Decision::Allow);
        if let Some(answerer) = self.answerers.get(&id) {
            answerer.answer(&request_id, decision)?;
        }
        self.reg
            .apply_event(id, &AgentEvent::ApprovalResolved { approved });
        Ok(())
    }

    /// Terminate an agent's process. Its reader thread then sees EOF and the
    /// agent transitions to Failed (via the EOF safety net). No-op if unknown.
    pub fn kill(&mut self, id: AgentId) {
        if let Some(pid) = self.pids.remove(&id) {
            // std has no signal API; shell out to `kill` (dependency-free).
            let _ = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
        }
        self.answerers.remove(&id);
    }

    /// Join all reader threads (call once agents are terminal / on shutdown).
    pub fn join(self) {
        drop(self.answerers); // close stdin handles
        for h in self.readers {
            let _ = h.join();
        }
    }
}
