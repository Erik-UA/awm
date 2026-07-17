//! The agent registry — the core's mutable model of every live agent.
//!
//! It ingests normalized [`AgentEvent`]s (via [`Registry::apply_event`]), keeps
//! each agent's state/tokens/recent-output, and tracks approval waits so the
//! layout engine can promote the oldest-blocked agent to the master zone.

use awm_proto::{
    AgentEvent, AgentId, AgentMeta, AgentState, AgentView, ApprovalCtx, Tags, TokenUsage,
};
use std::collections::{HashMap, VecDeque};

/// How many recent output lines to keep per agent for the pane body.
const TAIL_CAP: usize = 200;

/// Everything the core tracks for one agent.
pub struct AgentRecord {
    pub meta: AgentMeta,
    pub state: AgentState,
    pub tokens: TokenUsage,
    /// Recent output lines (ring, oldest first).
    pub tail: VecDeque<String>,
    /// Logical wait order when blocked (lower = waiting longer); `None` unless
    /// currently `BlockedOnApproval`.
    pub blocked_since: Option<u64>,
    /// The pending approval context while blocked (carries the `request_id`).
    pub pending: Option<ApprovalCtx>,
}

impl AgentRecord {
    fn view(&self) -> AgentView {
        AgentView {
            meta: self.meta.clone(),
            state: self.state,
            tokens: self.tokens,
            tail: self.tail.iter().cloned().collect(),
        }
    }

    fn push_tail(&mut self, line: String) {
        self.tail.push_back(line);
        while self.tail.len() > TAIL_CAP {
            self.tail.pop_front();
        }
    }
}

/// The set of agents plus focus and an internal wait clock.
#[derive(Default)]
pub struct Registry {
    agents: HashMap<AgentId, AgentRecord>,
    /// Stable insertion order (drives stack ordering and focus cycling).
    order: Vec<AgentId>,
    focus: Option<AgentId>,
    /// Monotonic counter stamped onto each block, so blocked agents sort by wait.
    clock: u64,
    next_id: u32,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    /// Allocate a fresh, stable agent id.
    pub fn alloc_id(&mut self) -> AgentId {
        let id = AgentId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Register a new (Idle) agent. Focuses it if nothing is focused yet.
    pub fn add(&mut self, meta: AgentMeta) {
        let id = meta.id;
        self.agents.insert(
            id,
            AgentRecord {
                meta,
                state: AgentState::Idle,
                tokens: TokenUsage::default(),
                tail: VecDeque::new(),
                blocked_since: None,
                pending: None,
            },
        );
        self.order.push(id);
        if self.focus.is_none() {
            self.focus = Some(id);
        }
    }

    /// Fold one event into an agent's record.
    pub fn apply_event(&mut self, id: AgentId, event: &AgentEvent) {
        let tick = self.clock;
        let mut stamped = false;

        if let Some(rec) = self.agents.get_mut(&id) {
            // Terminal agents absorb everything — including tail side effects.
            // (e.g. the reader's EOF `Finished{ok:false}` after a clean finish.)
            if rec.state.is_terminal() {
                return;
            }
            rec.state = rec.state.apply(event);

            match event {
                AgentEvent::Tokens(t) => rec.tokens = *t,
                AgentEvent::ApprovalRequested(ctx) => {
                    rec.pending = Some(ctx.clone());
                    rec.blocked_since = Some(tick);
                    rec.meta.urgent = true;
                    stamped = true;
                }
                AgentEvent::ApprovalResolved { .. } => {
                    rec.pending = None;
                    rec.blocked_since = None;
                    rec.meta.urgent = false;
                }
                _ => {}
            }

            if let Some(line) = describe(event) {
                rec.push_tail(line);
            }
        }

        if stamped {
            self.clock += 1;
        }
    }

    /// Render DTOs in roster order.
    pub fn views(&self) -> Vec<AgentView> {
        self.order
            .iter()
            .filter_map(|id| self.agents.get(id))
            .map(AgentRecord::view)
            .collect()
    }

    /// Blocked agents, oldest-waiting first (drives urgent → master and triage).
    pub fn blocked_ordered(&self) -> Vec<AgentId> {
        let mut blocked: Vec<(&AgentId, u64)> = self
            .order
            .iter()
            .filter_map(|id| {
                let rec = self.agents.get(id)?;
                rec.blocked_since.map(|since| (id, since))
            })
            .collect();
        blocked.sort_by_key(|(_, since)| *since);
        blocked.into_iter().map(|(id, _)| *id).collect()
    }

    pub fn order(&self) -> &[AgentId] {
        &self.order
    }

    pub fn focus(&self) -> Option<AgentId> {
        self.focus
    }

    pub fn set_focus(&mut self, id: AgentId) {
        if self.agents.contains_key(&id) {
            self.focus = Some(id);
        }
    }

    pub fn record(&self, id: AgentId) -> Option<&AgentRecord> {
        self.agents.get(&id)
    }

    pub fn pending_request_id(&self, id: AgentId) -> Option<String> {
        self.agents
            .get(&id)?
            .pending
            .as_ref()
            .map(|c| c.request_id.clone())
    }

    /// Move focus to the next/previous agent in roster order (wraps).
    pub fn focus_step(&mut self, delta: isize) {
        if self.order.is_empty() {
            return;
        }
        let cur = self
            .focus
            .and_then(|f| self.order.iter().position(|i| *i == f))
            .unwrap_or(0) as isize;
        let len = self.order.len() as isize;
        let next = ((cur + delta) % len + len) % len;
        self.focus = Some(self.order[next as usize]);
    }

    /// Toggle tag slot `n` (1-based) on an agent.
    pub fn toggle_tag(&mut self, id: AgentId, n: u8) {
        if let Some(rec) = self.agents.get_mut(&id) {
            rec.meta.tags.toggle(Tags::slot(n));
        }
    }

    /// Whether every agent has reached a terminal state (Done/Failed).
    pub fn all_terminal(&self) -> bool {
        !self.agents.is_empty() && self.agents.values().all(|r| r.state.is_terminal())
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

/// A short human-readable tail line for an event, or `None` to record nothing.
fn describe(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::Started { model, .. } => Some(format!("● session started ({model})")),
        AgentEvent::ToolStarted { name } => Some(format!("→ {name}")),
        AgentEvent::ApprovalRequested(ctx) => Some(format!(
            "⏸ approval: {} {}",
            ctx.tool,
            ctx.description.as_deref().unwrap_or("")
        )),
        AgentEvent::ApprovalResolved { approved } => {
            Some(if *approved { "✓ approved".into() } else { "✗ denied".into() })
        }
        AgentEvent::Finished { ok } => {
            Some(if *ok { "● done".into() } else { "● failed".into() })
        }
        // Thinking / Tokens / Noise are too noisy or invisible for the tail.
        _ => None,
    }
}
