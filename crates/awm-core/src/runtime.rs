//! The runtime engine — spawns agents, streams their events into the registry,
//! and answers approvals over the control channel.
//!
//! Each agent gets a reader thread that blocks on [`StreamJsonRunner::read`],
//! parses bytes into [`AgentEvent`]s, and forwards them over an mpsc channel.
//! The owning (UI) thread holds an [`Answerer`] per agent so it can approve/deny
//! a gate while the reader thread is blocked — no deadlock.

use crate::registry::Registry;
use awm_parser::{Spawn, StreamParser};
use awm_proto::{AgentEvent, AgentId, AgentMeta, Tags};
use awm_pty::{Answerer, CommandSpec, Decision, StreamJsonRunner};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

/// Name prefix stamped on a spawned sub-agent's pane. The frozen `AgentView`
/// carries no parent field, so the name is the marker channel — the TUI detects
/// this prefix to style a sub-agent pane (keep it in sync with awm-tui).
pub const SUBAGENT_PREFIX: &str = "\u{21b3} "; // "↳ "

/// An event tagged with the agent (process) it came from, plus sub-agent routing
/// pulled off the stream out-of-band (see [`awm_parser::Routed`]).
pub struct CoreEvent {
    pub id: AgentId,
    pub event: AgentEvent,
    /// The `Task` tool id whose sub-agent produced this event, if any.
    pub parent_tool_use_id: Option<String>,
    /// Set when this event is a `Task` call that spawns a sub-agent.
    pub spawn: Option<Spawn>,
}

/// Owns the model and every agent's I/O plumbing.
pub struct Engine {
    reg: Registry,
    tx: Sender<CoreEvent>,
    rx: Receiver<CoreEvent>,
    answerers: HashMap<AgentId, Answerer>,
    pids: HashMap<AgentId, u32>,
    readers: Vec<JoinHandle<()>>,
    /// `Task` tool id → the sub-agent pane it spawned. Routes nested events.
    child_by_tool: HashMap<String, AgentId>,
    /// Root (process) id → every sub-agent pane spawned under it, so they can be
    /// retired together when the root's turn ends.
    descendants: HashMap<AgentId, Vec<AgentId>>,
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
            child_by_tool: HashMap::new(),
            descendants: HashMap::new(),
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
        persistent: bool,
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
                        while let Some(routed) = parser.next_routed() {
                            // For a persistent agent a per-turn `result` is not
                            // the end — it just means "ready for the next turn".
                            let event = match routed.event {
                                AgentEvent::Finished { ok } if persistent => {
                                    AgentEvent::TurnEnded { ok }
                                }
                                other => other,
                            };
                            if tx
                                .send(CoreEvent {
                                    id,
                                    event,
                                    parent_tool_use_id: routed.parent_tool_use_id,
                                    spawn: routed.spawn,
                                })
                                .is_err()
                            {
                                return; // engine dropped
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            // Process exit is the real session terminal. The exit code tells a
            // clean finish (Done) from a crash/kill (Failed); if the agent is
            // already terminal, this is absorbed.
            let code = runner.wait().unwrap_or(-1);
            let _ = tx.send(CoreEvent {
                id,
                event: AgentEvent::Finished { ok: code == 0 },
                parent_tool_use_id: None,
                spawn: None,
            });
        });
        self.readers.push(handle);
        Ok(id)
    }

    /// Drain all currently-ready events into the registry. Returns how many.
    pub fn pump(&mut self) -> usize {
        let mut n = 0;
        while let Ok(ce) = self.rx.try_recv() {
            self.route(ce);
            n += 1;
        }
        n
    }

    /// Wait up to `timeout` for at least one event, then drain the rest.
    pub fn pump_blocking(&mut self, timeout: Duration) -> usize {
        match self.rx.recv_timeout(timeout) {
            Ok(ce) => {
                self.route(ce);
                1 + self.pump()
            }
            Err(_) => 0,
        }
    }

    /// Fold one event into the registry, routing sub-agent output to its own
    /// pane. A `Task` call mints a child pane adjacent to the spawner; nested
    /// events (tagged with `parent_tool_use_id`) land in that child; when the
    /// root process's turn ends, its sub-agent panes are retired.
    fn route(&mut self, ce: CoreEvent) {
        // Resolve the target pane: a sub-agent's own id if this event belongs to
        // one, else the root process id.
        let target = ce
            .parent_tool_use_id
            .as_ref()
            .and_then(|tid| self.child_by_tool.get(tid).copied())
            .unwrap_or(ce.id);

        // A `Task` tool_use spawns a sub-agent pane, inserted right after the
        // pane that spawned it so it sits adjacent in the stack.
        if let Some(spawn) = &ce.spawn {
            if !self.child_by_tool.contains_key(&spawn.tool_use_id) {
                let child = self.reg.alloc_id();
                let cwd = self
                    .reg
                    .record(target)
                    .map(|r| r.meta.cwd.clone())
                    .unwrap_or_default();
                let name = format!("{SUBAGENT_PREFIX}{}", spawn.description);
                self.reg.add_after(target, AgentMeta::new(child, name, cwd, 0));
                self.child_by_tool.insert(spawn.tool_use_id.clone(), child);
                self.descendants.entry(ce.id).or_default().push(child);
            }
        }

        self.reg.apply_event(target, &ce.event);

        // When the root process's own turn ends, retire its sub-agent panes.
        // (Root output has no `parent_tool_use_id`.)
        if ce.parent_tool_use_id.is_none()
            && matches!(
                ce.event,
                AgentEvent::TurnEnded { .. } | AgentEvent::Finished { .. }
            )
        {
            if let Some(children) = self.descendants.remove(&ce.id) {
                let removed: HashSet<AgentId> = children.iter().copied().collect();
                for child in children {
                    self.reg.remove(child);
                }
                self.child_by_tool.retain(|_, v| !removed.contains(v));
            }
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

    /// Send a follow-up message to a live agent (dialogue). Writes a stream-json
    /// user message on its stdin and echoes it into the agent's window. The
    /// agent's reply arrives as `Message` events on its stream.
    pub fn send_message(&mut self, id: AgentId, text: &str) -> std::io::Result<()> {
        // A finished (process-exited) agent can't receive input.
        if self.reg.record(id).map(|r| r.state.is_terminal()).unwrap_or(true) {
            self.reg
                .push_note(id, "\u{25b7} (agent finished — can't message)".to_string());
            return Ok(());
        }
        if let Some(answerer) = self.answerers.get(&id) {
            answerer.send_prompt(text)?;
            self.reg.push_note(id, format!("\u{25b7} you: {text}"));
        }
        Ok(())
    }

    /// Switch a live agent's permission mode (e.g. into `plan`). Sends the
    /// control_request and optimistically updates the shown mode.
    pub fn set_permission_mode(&mut self, id: AgentId, mode: &str) -> std::io::Result<()> {
        if let Some(answerer) = self.answerers.get(&id) {
            // Best-effort: a finished agent's stdin is closed, but the optimistic
            // UI update below should still reflect the requested mode.
            let _ = answerer.set_permission_mode(mode);
        }
        self.reg.set_permission_mode(id, mode);
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

    /// Kill every agent. Persistent agents won't exit on their own, so call this
    /// before dropping the engine (e.g. on quit) to avoid lingering processes.
    pub fn shutdown(&mut self) {
        let ids: Vec<AgentId> = self.pids.keys().copied().collect();
        for id in ids {
            self.kill(id);
        }
    }

    /// Join all reader threads (call once agents are terminal / on shutdown).
    pub fn join(self) {
        drop(self.answerers); // close stdin handles
        for h in self.readers {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ev(
        id: AgentId,
        event: AgentEvent,
        parent: Option<&str>,
        spawn: Option<Spawn>,
    ) -> CoreEvent {
        CoreEvent {
            id,
            event,
            parent_tool_use_id: parent.map(str::to_string),
            spawn,
        }
    }

    fn tail_has(engine: &Engine, id: AgentId, needle: &str) -> bool {
        engine
            .registry()
            .record(id)
            .map(|r| r.tail.iter().any(|l| l.text.contains(needle)))
            .unwrap_or(false)
    }

    /// A `Task` spawn mints a child pane adjacent to its parent; nested output
    /// lands in the child (not the parent); the child is retired when the root
    /// process's turn ends.
    #[test]
    fn task_spawn_routes_to_child_pane_and_retires_it() {
        let mut engine = Engine::new();
        let root = engine.registry_mut().alloc_id();
        engine
            .registry_mut()
            .add(AgentMeta::new(root, "root", PathBuf::from("/tmp"), 0));

        // 1) The Task call spawns a sub-agent.
        engine.route(ev(
            root,
            AgentEvent::ToolStarted {
                name: "Task".into(),
                summary: "find handlers".into(),
            },
            None,
            Some(Spawn {
                tool_use_id: "toolu_1".into(),
                description: "find handlers".into(),
            }),
        ));

        // The child pane sits right after root, marked with the ↳ prefix.
        let order = engine.registry().order().to_vec();
        assert_eq!(order.len(), 2, "child pane minted");
        assert_eq!(order[0], root);
        let child = order[1];
        let child_name = engine.registry().record(child).unwrap().meta.name.clone();
        assert!(child_name.starts_with(SUBAGENT_PREFIX), "name: {child_name}");
        assert!(child_name.contains("find handlers"));

        // 2) The nested sub-agent message lands in the child, not the root.
        engine.route(ev(
            root,
            AgentEvent::Message {
                text: "Searching for handler definitions...".into(),
            },
            Some("toolu_1"),
            None,
        ));
        assert!(tail_has(&engine, child, "Searching for handler"));
        assert!(!tail_has(&engine, root, "Searching for handler"));

        // 3) The root's own follow-up lands in the root.
        engine.route(ev(
            root,
            AgentEvent::Message {
                text: "The subagent found 4 handlers.".into(),
            },
            None,
            None,
        ));
        assert!(tail_has(&engine, root, "found 4 handlers"));

        // 4) The root's turn ends → the sub-agent pane is retired.
        engine.route(ev(root, AgentEvent::Finished { ok: true }, None, None));
        assert_eq!(engine.registry().order(), &[root], "child retired");
    }

    /// A duplicate Task frame for the same tool id must not mint a second pane.
    #[test]
    fn duplicate_task_spawn_is_idempotent() {
        let mut engine = Engine::new();
        let root = engine.registry_mut().alloc_id();
        engine
            .registry_mut()
            .add(AgentMeta::new(root, "root", PathBuf::from("/tmp"), 0));

        let spawn = || {
            Some(Spawn {
                tool_use_id: "toolu_1".into(),
                description: "scan".into(),
            })
        };
        let tool = || AgentEvent::ToolStarted {
            name: "Task".into(),
            summary: "scan".into(),
        };
        engine.route(ev(root, tool(), None, spawn()));
        engine.route(ev(root, tool(), None, spawn()));
        assert_eq!(engine.registry().order().len(), 2, "only one child pane");
    }
}
