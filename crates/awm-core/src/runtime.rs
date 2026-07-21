//! The runtime engine — spawns agents, streams their events into the registry,
//! and answers approvals over the control channel.
//!
//! Each agent gets a reader thread that blocks on [`StreamJsonRunner::read`],
//! parses bytes into [`AgentEvent`]s, and forwards them over an mpsc channel.
//! The owning (UI) thread holds an [`Answerer`] per agent so it can approve/deny
//! a gate while the reader thread is blocked — no deadlock.

use crate::registry::Registry;
use awm_parser::{Spawn, StreamParser};
use awm_proto::{AgentEvent, AgentId, AgentMeta, AgentState, Tags};
use awm_pty::{Answerer, CommandSpec, Decision, StreamJsonRunner};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

/// Name prefix stamped on a spawned sub-agent's pane. The frozen `AgentView`
/// carries no parent field, so the name is the marker channel — the TUI detects
/// this prefix to style a sub-agent pane (keep it in sync with awm-tui).
pub const SUBAGENT_PREFIX: &str = "\u{21b3} "; // "↳ "

/// Open the per-agent raw-capture file when `AWM_CAPTURE_DIR` is set, else `None`.
/// Diagnostic only: dumps the verbatim stdout stream for `id` to
/// `<AWM_CAPTURE_DIR>/agent-<id>.jsonl` (created, truncated). Best-effort.
fn capture_file(id: AgentId) -> Option<std::fs::File> {
    let dir = std::env::var_os("AWM_CAPTURE_DIR")?;
    let dir = std::path::PathBuf::from(dir);
    let _ = std::fs::create_dir_all(&dir);
    std::fs::File::create(dir.join(format!("agent-{}.jsonl", id.0))).ok()
}

/// An event tagged with the agent (process) it came from, plus sub-agent routing
/// pulled off the stream out-of-band (see [`awm_parser::Routed`]).
pub struct CoreEvent {
    pub id: AgentId,
    pub event: AgentEvent,
    /// The `Task` tool id whose sub-agent produced this event, if any.
    pub parent_tool_use_id: Option<String>,
    /// The `tool_use.id` of a `ToolStarted` event — records which pane owns the
    /// tool so its later approval gate routes there.
    pub tool_use_id: Option<String>,
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
    /// `tool_use.id` → the pane that owns that tool call, so a later
    /// `can_use_tool` gate (which references the id) routes to that pane.
    tool_owner: HashMap<String, AgentId>,
    /// Sub-agent pane → its root process id (which owns the real Answerer), so a
    /// child pane's approval answer is written to the root process's stdin.
    parent_of: HashMap<AgentId, AgentId>,
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
            tool_owner: HashMap::new(),
            parent_of: HashMap::new(),
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
        // Agents that run the control-channel handshake are real Claude sessions —
        // the only ones `claude --resume` can bring back. Mocks are not resumable.
        self.reg.set_resumable(id, handshake);
        self.attach(id, spec, prompt, handshake, persistent)?;
        Ok(id)
    }

    /// Re-attach a LIVE process to an already-restored pane. Unlike [`Engine::spawn`]
    /// this allocates no id and adds no record — the pane already exists (from
    /// [`crate::Registry::restore`]). Used by session restore to bring a persisted
    /// agent back with `claude --resume <session_id>` while keeping its restored
    /// transcript. The pane is first reactivated so the state machine accepts the
    /// resumed process's events; new output appends to the same tail.
    pub fn resume_agent(
        &mut self,
        id: AgentId,
        spec: CommandSpec,
        prompt: Option<String>,
        handshake: bool,
        persistent: bool,
    ) -> std::io::Result<()> {
        self.reg.reactivate(id);
        self.attach(id, spec, prompt, handshake, persistent)
    }

    /// Whether a live process is currently attached to `id` (so callers can avoid
    /// double-resuming an already-live pane).
    pub fn is_live(&self, id: AgentId) -> bool {
        self.answerers.contains_key(&id)
    }

    /// Close the active project (screen): terminate its agents' processes and drop
    /// their panes. With more than one screen the project is removed and the active
    /// one switches to a neighbour; on the SOLE remaining screen the project is kept
    /// but emptied (there is always at least one screen). Always takes effect.
    pub fn close_active_project(&mut self) -> bool {
        let pid = self.reg.active();
        for id in self.reg.agents_in(pid) {
            self.kill(id); // SIGTERM the process (no-op for sub-agent panes)
        }
        if self.reg.projects().len() > 1 {
            self.reg.remove_project(pid)
        } else {
            self.reg.clear_project(pid); // last screen — empty it in place
            true
        }
    }

    /// Close a single focused agent pane: SIGTERM its process (a no-op for a
    /// sub-agent pane, which shares the root process), retire any sub-agent panes
    /// it owns, drop the pane, and clean the parent/child bookkeeping. Focus
    /// re-points via [`Registry::remove`].
    ///
    /// A sub-agent has no process of its own, so closing its pane is a UI dismiss:
    /// the pane and its bookkeeping go away and further output falls back to the
    /// root pane; the inner work (running inside the root) is not force-stopped.
    /// Closing a top-level agent kills the real process and takes its sub-agent
    /// panes with it — they can't outlive it — mirroring the `Finished` cleanup.
    pub fn close_agent(&mut self, id: AgentId) {
        // Retire descendant sub-agent panes (they share the process being killed).
        if let Some(children) = self.descendants.remove(&id) {
            let removed: HashSet<AgentId> = children.iter().copied().collect();
            for child in children {
                self.reg.remove(child);
            }
            self.child_by_tool.retain(|_, v| !removed.contains(v));
            self.parent_of.retain(|c, _| !removed.contains(c));
            self.tool_owner.retain(|_, v| !removed.contains(v));
        }
        // If this pane is itself a sub-agent, detach it from its parent's maps.
        if self.parent_of.remove(&id).is_some() {
            if let Some(v) = self.descendants.values_mut().find(|v| v.contains(&id)) {
                v.retain(|c| *c != id);
            }
            self.child_by_tool.retain(|_, v| *v != id);
            self.tool_owner.retain(|_, v| *v != id);
        }
        self.kill(id); // SIGTERM if it has a live process; no-op otherwise
        self.reg.remove(id); // drop the pane + re-point focus
    }

    /// Start a process for `spec`, wire its answerer/pid, and spawn the reader
    /// thread that forwards its events tagged with `id`. Shared by spawn/resume.
    fn attach(
        &mut self,
        id: AgentId,
        spec: CommandSpec,
        prompt: Option<String>,
        handshake: bool,
        persistent: bool,
    ) -> std::io::Result<()> {
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
            // Optional raw-stream capture for diagnostics: when `AWM_CAPTURE_DIR`
            // is set, tee every raw stdout chunk (verbatim, pre-parse) to
            // `<dir>/agent-<id>.jsonl`. All of one process's bytes — including its
            // sub-agents' interleaved frames — land in one file: exactly what the
            // parser/router saw. Best-effort; capture errors never disturb the run.
            let mut capture = capture_file(id);
            loop {
                match runner.read() {
                    Ok(chunk) if chunk.is_empty() => break, // EOF
                    Ok(chunk) => {
                        if let Some(f) = capture.as_mut() {
                            use std::io::Write;
                            let _ = f.write_all(&chunk);
                            let _ = f.flush();
                        }
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
                                    tool_use_id: routed.tool_use_id,
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
                tool_use_id: None,
                spawn: None,
            });
        });
        self.readers.push(handle);
        Ok(())
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
        // Resolve the target pane. An approval gate carries no `parent_tool_use_id`
        // (the `can_use_tool` envelope lacks it), so route it by the owning tool:
        // its `ctx.tool_use_id` is the id of a `tool_use` we already routed to a
        // pane. Other events route by `parent_tool_use_id` to the sub-agent pane,
        // else to the root process.
        let target = match &ce.event {
            AgentEvent::ApprovalRequested(ctx) => ctx
                .tool_use_id
                .as_ref()
                .and_then(|tid| self.tool_owner.get(tid).copied())
                .unwrap_or(ce.id),
            _ => ce
                .parent_tool_use_id
                .as_ref()
                .and_then(|tid| self.child_by_tool.get(tid).copied())
                .unwrap_or(ce.id),
        };

        // A `Task`/`Agent` tool_use spawns a sub-agent pane, inserted right after
        // the pane that spawned it so it sits adjacent in the stack.
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
                self.parent_of.insert(child, ce.id);
            }
        }

        // Remember which pane owns this tool call, so its later approval routes here.
        if matches!(ce.event, AgentEvent::ToolStarted { .. }) {
            if let Some(tid) = &ce.tool_use_id {
                self.tool_owner.insert(tid.clone(), target);
            }
        }

        self.reg.apply_event(target, &ce.event);

        // Retire sub-agent panes only when the root PROCESS exits (EOF `Finished`),
        // not on a per-turn `TurnEnded`. A turn end is not a session end: a
        // sub-agent — especially a background/async one launched via the `Agent`
        // tool — outlives the parent's turn (the tool returns immediately and the
        // parent goes idle), so retiring on `TurnEnded` would collapse its pane
        // before it does any work. (Root output has no `parent_tool_use_id`.)
        if ce.parent_tool_use_id.is_none()
            && matches!(ce.event, AgentEvent::Finished { .. })
        {
            if let Some(children) = self.descendants.remove(&ce.id) {
                let removed: HashSet<AgentId> = children.iter().copied().collect();
                for child in children {
                    self.reg.remove(child);
                }
                self.child_by_tool.retain(|_, v| !removed.contains(v));
                self.parent_of.retain(|c, _| !removed.contains(c));
                self.tool_owner.retain(|_, v| !removed.contains(v));
            }
        }
    }

    /// Answer the pending approval for `id`. Writes the `control_response` on the
    /// agent's stdin and — because we generate that response ourselves, so it
    /// never comes back on stdout — synthesizes the matching `ApprovalResolved`
    /// into the registry to unblock the agent.
    ///
    /// `id` may be a sub-agent pane, which has no process of its own: its gate is
    /// answered on the ROOT process's Answerer (all sub-agents share it), using the
    /// pane's own pending `request_id`.
    pub fn answer(&mut self, id: AgentId, decision: Decision) -> std::io::Result<()> {
        let Some(request_id) = self.reg.pending_request_id(id) else {
            return Ok(()); // nothing pending
        };
        // A sub-agent pane's stdin lives on its root process.
        let proc = if self.answerers.contains_key(&id) {
            id
        } else {
            self.parent_of.get(&id).copied().unwrap_or(id)
        };
        let approved = matches!(decision, Decision::Allow | Decision::AllowWith(_));
        if let Some(answerer) = self.answerers.get(&proc) {
            answerer.answer(&request_id, decision)?;
        }
        self.reg
            .apply_event(id, &AgentEvent::ApprovalResolved { approved });
        // If the user typed a message while this pane was blocked, deliver it now
        // as a real user turn — the gate is resolved, so the session accepts input
        // again (whether we allowed or denied). Sub-agent panes flush on the root.
        if let Some(text) = self.reg.take_pending_message(id) {
            if let Some(answerer) = self.answerers.get(&proc) {
                let _ = answerer.send_prompt(&text);
            }
        }
        Ok(())
    }

    /// Interrupt an agent's current turn **without ending the session** (the SDK
    /// `interrupt` control_request — like pressing `Esc` in claude). Only a live,
    /// actively-`Working` agent can be interrupted; idle/blocked/terminal agents
    /// are a no-op (blocked agents are answered with `y`/`n`, not interrupted). A
    /// sub-agent pane's interrupt is written to its root process's Answerer.
    pub fn interrupt(&mut self, id: AgentId) -> std::io::Result<()> {
        let working = self
            .reg
            .record(id)
            .map(|r| matches!(r.state, AgentState::Working))
            .unwrap_or(false);
        if !working {
            return Ok(());
        }
        let proc = if self.answerers.contains_key(&id) {
            id
        } else {
            self.parent_of.get(&id).copied().unwrap_or(id)
        };
        if let Some(answerer) = self.answerers.get(&proc) {
            answerer.interrupt()?;
            self.reg.push_note(id, "\u{238b} interrupted".to_string());
        }
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
        // A blocked agent is mid-`can_use_tool`, waiting for a `control_response`,
        // not a `user` turn — queuing the text and flushing it when the gate
        // resolves keeps the dialogue working without racing the control channel.
        if self.reg.record(id).map(|r| r.state.is_blocked()).unwrap_or(false) {
            self.reg.queue_message(id, text);
            self.reg.push_note(id, format!("\u{25b7} you: {text}"));
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
            tool_use_id: None,
            spawn,
        }
    }

    /// A `ToolStarted` event carrying its `tool_use_id` (for gate correlation).
    fn tool_ev(id: AgentId, name: &str, tool_use_id: &str, parent: Option<&str>) -> CoreEvent {
        CoreEvent {
            id,
            event: AgentEvent::ToolStarted {
                name: name.into(),
                summary: String::new(),
                input: None,
            },
            parent_tool_use_id: parent.map(str::to_string),
            tool_use_id: Some(tool_use_id.to_string()),
            spawn: None,
        }
    }

    /// An `ApprovalRequested` gate referencing `tool_use_id` (no parent id, as in
    /// the real `can_use_tool` envelope).
    fn gate_ev(id: AgentId, tool_use_id: &str, request_id: &str) -> CoreEvent {
        CoreEvent {
            id,
            event: AgentEvent::ApprovalRequested(awm_proto::ApprovalCtx {
                tool: "Bash".into(),
                input: serde_json::Value::Null,
                request_id: request_id.into(),
                tool_use_id: Some(tool_use_id.into()),
                description: None,
                decision_reason: None,
                diff: None,
                suggestions: Vec::new(),
            }),
            parent_tool_use_id: None,
            tool_use_id: None,
            spawn: None,
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
                input: None,
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
            input: None,
        };
        engine.route(ev(root, tool(), None, spawn()));
        engine.route(ev(root, tool(), None, spawn()));
        assert_eq!(engine.registry().order().len(), 2, "only one child pane");
    }

    /// A sub-agent pane must survive the parent's per-turn `TurnEnded` (background
    /// agents outlive the turn) and be retired only when the root process exits.
    #[test]
    fn subagent_pane_survives_turn_end_retires_on_process_exit() {
        let mut engine = Engine::new();
        let root = engine.registry_mut().alloc_id();
        engine
            .registry_mut()
            .add(AgentMeta::new(root, "root", PathBuf::from("/tmp"), 0));

        engine.route(ev(
            root,
            AgentEvent::ToolStarted { name: "Agent".into(), summary: "bg".into(), input: None },
            None,
            Some(Spawn { tool_use_id: "toolu_1".into(), description: "bg".into() }),
        ));
        let child = *engine.child_by_tool.get("toolu_1").unwrap();
        assert!(engine.registry().order().contains(&child), "child minted");

        // Parent's turn ends (it launched a background agent and went idle) — the
        // sub-agent pane must NOT be retired.
        engine.route(ev(root, AgentEvent::TurnEnded { ok: true }, None, None));
        assert!(
            engine.registry().order().contains(&child),
            "child survives TurnEnded"
        );

        // The root process exits — now the pane is retired and maps cleaned.
        engine.route(ev(root, AgentEvent::Finished { ok: true }, None, None));
        assert!(!engine.registry().order().contains(&child), "child retired on exit");
        assert!(engine.child_by_tool.is_empty());
        assert!(engine.parent_of.is_empty());
    }

    /// `close_agent` on a sub-agent pane dismisses only that pane and its
    /// bookkeeping; the root and its other sub-agent survive (sub-agents have no
    /// process, so nothing is SIGTERMed).
    #[test]
    fn close_agent_on_subagent_drops_only_that_pane() {
        let mut engine = Engine::new();
        let root = engine.registry_mut().alloc_id();
        engine
            .registry_mut()
            .add(AgentMeta::new(root, "root", PathBuf::from("/tmp"), 0));

        for (tid, desc) in [("toolu_a1", "parser"), ("toolu_a2", "core")] {
            engine.route(ev(
                root,
                AgentEvent::ToolStarted { name: "Agent".into(), summary: desc.into(), input: None },
                None,
                Some(Spawn { tool_use_id: tid.into(), description: desc.into() }),
            ));
        }
        let child1 = *engine.child_by_tool.get("toolu_a1").unwrap();
        let child2 = *engine.child_by_tool.get("toolu_a2").unwrap();

        engine.close_agent(child1);

        assert!(!engine.registry().order().contains(&child1), "child1 dismissed");
        assert!(engine.registry().order().contains(&root), "root survives");
        assert!(engine.registry().order().contains(&child2), "sibling survives");
        // child1's maps are cleaned; child2's are intact.
        assert!(!engine.parent_of.contains_key(&child1));
        assert!(!engine.child_by_tool.values().any(|v| *v == child1));
        assert_eq!(engine.descendants.get(&root), Some(&vec![child2]));
        assert_eq!(engine.parent_of.get(&child2), Some(&root));
    }

    /// `close_agent` on a top-level agent retires its sub-agent panes with it (they
    /// share the killed process) and leaves the routing maps empty.
    #[test]
    fn close_agent_on_root_retires_its_subagents() {
        let mut engine = Engine::new();
        let root = engine.registry_mut().alloc_id();
        engine
            .registry_mut()
            .add(AgentMeta::new(root, "root", PathBuf::from("/tmp"), 0));

        engine.route(ev(
            root,
            AgentEvent::ToolStarted { name: "Agent".into(), summary: "bg".into(), input: None },
            None,
            Some(Spawn { tool_use_id: "toolu_1".into(), description: "bg".into() }),
        ));
        let child = *engine.child_by_tool.get("toolu_1").unwrap();

        engine.close_agent(root);

        assert!(!engine.registry().order().contains(&root), "root removed");
        assert!(!engine.registry().order().contains(&child), "sub-agent removed with it");
        assert!(engine.child_by_tool.is_empty());
        assert!(engine.parent_of.is_empty());
        assert!(engine.descendants.is_empty());
    }

    /// Two sub-agents each blocking on a gate: each approval must land in its OWN
    /// child pane (not collide on the root), and each pane maps back to the root.
    #[test]
    fn subagent_gates_route_to_their_own_panes() {
        let mut engine = Engine::new();
        let root = engine.registry_mut().alloc_id();
        engine
            .registry_mut()
            .add(AgentMeta::new(root, "root", PathBuf::from("/tmp"), 0));

        // Two Agent spawns → two child panes.
        for (tid, desc) in [("toolu_a1", "run parser tests"), ("toolu_a2", "run core tests")] {
            engine.route(ev(
                root,
                AgentEvent::ToolStarted { name: "Agent".into(), summary: desc.into(), input: None },
                None,
                Some(Spawn { tool_use_id: tid.into(), description: desc.into() }),
            ));
        }
        let order = engine.registry().order().to_vec();
        assert_eq!(order.len(), 3, "root + two children");
        let child1 = *engine.child_by_tool.get("toolu_a1").unwrap();
        let child2 = *engine.child_by_tool.get("toolu_a2").unwrap();

        // Each sub-agent runs an inner Bash tool (routed to its pane via the
        // spawn's parent_tool_use_id), then that tool blocks on a gate.
        engine.route(tool_ev(root, "Bash", "toolu_b1", Some("toolu_a1")));
        engine.route(tool_ev(root, "Bash", "toolu_b2", Some("toolu_a2")));
        engine.route(gate_ev(root, "toolu_b1", "req-1"));
        engine.route(gate_ev(root, "toolu_b2", "req-2"));

        // Each gate landed in its own child pane — not on the root.
        assert_eq!(engine.reg.pending_request_id(child1).as_deref(), Some("req-1"));
        assert_eq!(engine.reg.pending_request_id(child2).as_deref(), Some("req-2"));
        assert_eq!(engine.reg.pending_request_id(root), None, "root not blocked");
        assert!(engine.registry().record(child1).unwrap().meta.urgent);
        assert!(engine.registry().record(child2).unwrap().meta.urgent);

        // Each child pane maps back to the root process (for answering).
        assert_eq!(engine.parent_of.get(&child1), Some(&root));
        assert_eq!(engine.parent_of.get(&child2), Some(&root));

        // Answering a child pane clears ITS block (and doesn't panic without a
        // real Answerer — the write is a no-op in the test).
        engine.answer(child1, Decision::Allow).unwrap();
        assert_eq!(engine.reg.pending_request_id(child1), None, "child1 unblocked");
        assert_eq!(
            engine.reg.pending_request_id(child2).as_deref(),
            Some("req-2"),
            "child2 still blocked"
        );
    }
}
