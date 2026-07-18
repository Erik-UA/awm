//! The agent registry — the core's mutable model of every live agent.
//!
//! It ingests normalized [`AgentEvent`]s (via [`Registry::apply_event`]), keeps
//! each agent's state/tokens/recent-output, and tracks approval waits so the
//! layout engine can promote the oldest-blocked agent to the master zone.

use awm_proto::{
    AgentEvent, AgentId, AgentInfo, AgentMeta, AgentState, AgentView, ApprovalCtx, LineKind, Tags,
    TokenUsage, TranscriptLine,
};
use std::collections::{HashMap, VecDeque};

/// How many recent transcript lines to keep per agent for the pane body.
const TAIL_CAP: usize = 400;
/// Cap on how many lines of a single tool result to show (Claude-style).
const TOOL_RESULT_LINES: usize = 8;

/// Everything the core tracks for one agent.
pub struct AgentRecord {
    pub meta: AgentMeta,
    pub state: AgentState,
    pub tokens: TokenUsage,
    /// Recent transcript lines (ring, oldest first).
    pub tail: VecDeque<TranscriptLine>,
    /// Logical wait order when blocked (lower = waiting longer); `None` unless
    /// currently `BlockedOnApproval`.
    pub blocked_since: Option<u64>,
    /// The pending approval context while blocked (carries the `request_id`).
    pub pending: Option<ApprovalCtx>,
    /// Session metadata from `init` (model, mode, tools/skills/plugins…).
    pub info: Option<AgentInfo>,
    /// Whether the last tail line is an in-progress streamed reply.
    streaming: bool,
}

impl AgentRecord {
    fn view(&self) -> AgentView {
        AgentView {
            meta: self.meta.clone(),
            state: self.state,
            tokens: self.tokens,
            info: self.info.clone(),
            tail: self.tail.iter().cloned().collect(),
        }
    }

    fn push_line(&mut self, line: TranscriptLine) {
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
                info: None,
                streaming: false,
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
                AgentEvent::Info(i) => rec.info = Some(i.clone()),
                _ => {}
            }

            // Transcript, with live streaming of the in-progress reply.
            match event {
                // Grow the live reply line token-by-token.
                AgentEvent::MessageDelta { text } => {
                    if !rec.streaming {
                        rec.push_line(TranscriptLine::new(LineKind::Text, String::new()));
                        rec.streaming = true;
                    }
                    if let Some(last) = rec.tail.back_mut() {
                        last.text.push_str(text);
                    }
                }
                // The complete message finalizes (replaces) the streamed line;
                // without prior streaming it is just appended.
                AgentEvent::Message { text } => {
                    if rec.streaming {
                        if let Some(last) = rec.tail.back_mut() {
                            last.text = text.trim().to_string();
                        }
                        rec.streaming = false;
                    } else {
                        for line in transcript_lines(event) {
                            rec.push_line(line);
                        }
                    }
                }
                // Invisible events must NOT disturb an in-progress stream — the
                // stream's block-start/stop frames arrive as Noise between deltas.
                AgentEvent::Noise | AgentEvent::Tokens(_) | AgentEvent::Info(_) => {}
                // Any real line commits the live stream, then records itself.
                other => {
                    rec.streaming = false;
                    for line in transcript_lines(other) {
                        rec.push_line(line);
                    }
                }
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

    /// Append an out-of-band line to an agent's window (e.g. the user's own
    /// message in a dialogue). No state change.
    pub fn push_note(&mut self, id: AgentId, line: String) {
        if let Some(rec) = self.agents.get_mut(&id) {
            rec.push_line(TranscriptLine::new(LineKind::Note, line));
        }
    }

    /// Optimistically record a permission-mode switch for the status bar.
    pub fn set_permission_mode(&mut self, id: AgentId, mode: &str) {
        if let Some(rec) = self.agents.get_mut(&id) {
            match &mut rec.info {
                Some(info) => info.permission_mode = mode.to_string(),
                none => {
                    *none = Some(AgentInfo {
                        permission_mode: mode.to_string(),
                        ..Default::default()
                    })
                }
            }
        }
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

/// Turn an event into the transcript line(s) shown in the agent's window,
/// formatted like Claude Code (glyphs baked in; the TUI adds only color and
/// markdown styling). `Thinking`/`Tokens`/`Noise` record nothing.
fn transcript_lines(event: &AgentEvent) -> Vec<TranscriptLine> {
    use LineKind as K;
    match event {
        AgentEvent::Started { model, .. } => {
            vec![TranscriptLine::new(K::System, format!("● session started ({model})"))]
        }
        // The agent's reply — kept whole (newlines embedded) so the TUI can
        // render markdown with cross-line state (code fences).
        AgentEvent::Message { text } => {
            let t = text.trim();
            if t.is_empty() {
                Vec::new()
            } else {
                vec![TranscriptLine::new(K::Text, t)]
            }
        }
        AgentEvent::ToolStarted { name, summary } => {
            let text = if summary.is_empty() {
                format!("⏺ {name}")
            } else {
                format!("⏺ {name}({summary})")
            };
            vec![TranscriptLine::new(K::ToolCall, text)]
        }
        AgentEvent::ToolResult { output, is_error } => {
            let kind = if *is_error { K::ToolError } else { K::ToolResult };
            let lines: Vec<&str> = output.lines().collect();
            let mut out = Vec::new();
            for (i, line) in lines.iter().take(TOOL_RESULT_LINES).enumerate() {
                // First line gets the `⎿` branch; continuations are indented.
                let prefix = if i == 0 { "⎿ " } else { "  " };
                out.push(TranscriptLine::new(kind, format!("{prefix}{line}")));
            }
            let extra = lines.len().saturating_sub(TOOL_RESULT_LINES);
            if extra > 0 {
                out.push(TranscriptLine::new(kind, format!("  … +{extra} lines")));
            }
            if out.is_empty() {
                out.push(TranscriptLine::new(kind, "⎿ (no output)".to_string()));
            }
            out
        }
        AgentEvent::ApprovalRequested(ctx) => {
            let desc = ctx.description.as_deref().unwrap_or("");
            vec![TranscriptLine::new(
                K::Approval,
                format!("⏸ approval: {} {}", ctx.tool, desc).trim_end().to_string(),
            )]
        }
        AgentEvent::ApprovalResolved { approved } => vec![TranscriptLine::new(
            K::Note,
            if *approved { "✓ approved" } else { "✗ denied" },
        )],
        // The agent's reasoning, shown dimmed (Track C fills the text).
        AgentEvent::Thinking { text } => {
            let t = text.trim();
            if t.is_empty() {
                Vec::new()
            } else {
                vec![TranscriptLine::new(K::Thinking, format!("✻ {t}"))]
            }
        }
        // A turn ended but the session lives on — a divider, ready for the next.
        AgentEvent::TurnEnded { .. } => {
            vec![TranscriptLine::new(K::System, "─".repeat(24))]
        }
        AgentEvent::Finished { ok } => vec![TranscriptLine::new(
            K::System,
            if *ok { "● done" } else { "● failed" },
        )],
        _ => Vec::new(),
    }
}
