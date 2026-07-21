//! The agent registry — the core's mutable model of every live agent.
//!
//! It ingests normalized [`AgentEvent`]s (via [`Registry::apply_event`]), keeps
//! each agent's state/tokens/recent-output, and tracks approval waits so the
//! layout engine can promote the oldest-blocked agent to the master zone.

use crate::session::PaneKind;
use awm_proto::{
    AgentEvent, AgentId, AgentInfo, AgentMeta, AgentState, AgentView, ApprovalCtx, LineKind, Tags,
    TokenUsage, TranscriptLine,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

/// Opaque, stable identifier for a project (screen) within an `awm` session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProjectId(pub u32);

/// A project — one "screen" the user switches between. Each project owns a
/// disjoint set of agents (partition of the roster) plus its own focus, and is
/// defined by a human name and the working directory its agents run in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub cwd: PathBuf,
}

/// How many recent transcript lines to keep per agent for the pane body.
const TAIL_CAP: usize = 400;
/// Cap on how many lines of a single tool result to show (Claude-style).
const TOOL_RESULT_LINES: usize = 8;
/// Cap on how many diff lines to show under an edit tool call.
const DIFF_LINES: usize = 12;

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
    /// Whether this agent is a live-resumable Claude session (spawned with the
    /// control-channel handshake), as opposed to a mock. Only these are brought
    /// back live via `claude --resume` on restore — a mock's `session_id` (e.g.
    /// `"mock-1"`) is not a real Claude session.
    pub resumable: bool,
    /// Whether this pane is a Claude agent or an interactive shell console.
    /// Shells bypass the stream-json runtime (their PTY lives in the binary) and
    /// are re-spawned fresh on restore rather than resumed.
    pub kind: PaneKind,
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

/// The set of agents plus per-project focus and an internal wait clock.
///
/// Agents live in one global map (ids are unique session-wide) but are
/// partitioned into *projects* (screens) via [`Registry::project_of`]. The
/// render-facing accessors ([`Registry::views`], [`Registry::blocked_ordered`],
/// [`Registry::focus`], [`Registry::active_order`]) are scoped to the **active**
/// project, so switching projects re-scopes the whole layout for free.
pub struct Registry {
    agents: HashMap<AgentId, AgentRecord>,
    /// Stable insertion order across ALL projects (drives per-project stack
    /// ordering, after filtering by [`Registry::project_of`]).
    order: Vec<AgentId>,
    /// Monotonic counter stamped onto each block, so blocked agents sort by wait.
    clock: u64,
    next_id: u32,
    /// Every project (screen), in creation order (`switch_to` indexes this).
    projects: Vec<Project>,
    /// The currently-shown project.
    active: ProjectId,
    /// Which project each agent belongs to.
    project_of: HashMap<AgentId, ProjectId>,
    /// Focused agent per project, so switching screens preserves each screen's
    /// focus. `focus()` returns the active project's entry.
    focus_by_project: HashMap<ProjectId, AgentId>,
    next_project_id: u32,
}

impl Default for Registry {
    fn default() -> Self {
        let mut reg = Registry {
            agents: HashMap::new(),
            order: Vec::new(),
            clock: 0,
            next_id: 0,
            projects: Vec::new(),
            active: ProjectId(0),
            project_of: HashMap::new(),
            focus_by_project: HashMap::new(),
            next_project_id: 0,
        };
        // Every registry starts with one default project so there is always a
        // screen to spawn into. The binary renames it to the real cwd.
        let id = reg.add_project("main", PathBuf::new());
        reg.active = id;
        reg
    }
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

    /// A fresh Idle record for `meta`.
    fn fresh(meta: AgentMeta) -> AgentRecord {
        AgentRecord {
            meta,
            state: AgentState::Idle,
            tokens: TokenUsage::default(),
            tail: VecDeque::new(),
            blocked_since: None,
            pending: None,
            info: None,
            resumable: false,
            kind: PaneKind::Agent,
            streaming: false,
        }
    }

    /// Mark whether this agent is a live-resumable Claude session (see the field).
    pub fn set_resumable(&mut self, id: AgentId, resumable: bool) {
        if let Some(rec) = self.agents.get_mut(&id) {
            rec.resumable = resumable;
        }
    }

    /// Register a new (Idle) agent in the **active** project. Focuses it if that
    /// project has no focus yet.
    pub fn add(&mut self, meta: AgentMeta) {
        let proj = self.active;
        self.add_in(proj, None, meta);
    }

    /// Register a new interactive **shell** pane in the active project. Identical
    /// to [`Registry::add`] except the record is marked [`PaneKind::Shell`] so it
    /// persists/restores as a shell (re-spawned fresh) rather than an agent. Its
    /// live PTY is owned by the binary, not the [`crate::Engine`].
    pub fn add_shell(&mut self, meta: AgentMeta) {
        let id = meta.id;
        self.add(meta);
        if let Some(rec) = self.agents.get_mut(&id) {
            rec.kind = PaneKind::Shell;
        }
    }

    /// Register a new (Idle) agent immediately after `anchor` in roster order, so
    /// a spawned sub-agent lands adjacent to its parent in the stack. The new
    /// agent **inherits `anchor`'s project** (a background sub-agent must not leak
    /// onto whatever screen is currently active). Appends if `anchor` is unknown.
    pub fn add_after(&mut self, anchor: AgentId, meta: AgentMeta) {
        let proj = self.project_of.get(&anchor).copied().unwrap_or(self.active);
        self.add_in(proj, Some(anchor), meta);
    }

    /// Shared insert path: place `meta` in `proj`, either after `anchor` or at the
    /// end of the roster, and give `proj` a focus if it lacked one.
    fn add_in(&mut self, proj: ProjectId, anchor: Option<AgentId>, meta: AgentMeta) {
        let id = meta.id;
        self.agents.insert(id, Self::fresh(meta));
        match anchor.and_then(|a| self.order.iter().position(|i| *i == a)) {
            Some(pos) => self.order.insert(pos + 1, id),
            None => self.order.push(id),
        }
        self.project_of.insert(id, proj);
        self.focus_by_project.entry(proj).or_insert(id);
    }

    /// Remove an agent entirely — used to retire a finished sub-agent's pane once
    /// its parent's turn ends. Moves that project's focus to its first remaining
    /// agent if the removed one was focused. No-op if unknown.
    pub fn remove(&mut self, id: AgentId) {
        self.agents.remove(&id);
        self.order.retain(|i| *i != id);
        if let Some(proj) = self.project_of.remove(&id) {
            if self.focus_by_project.get(&proj) == Some(&id) {
                match self
                    .order
                    .iter()
                    .find(|i| self.project_of.get(i) == Some(&proj))
                    .copied()
                {
                    Some(next) => {
                        self.focus_by_project.insert(proj, next);
                    }
                    None => {
                        self.focus_by_project.remove(&proj);
                    }
                }
            }
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

    /// Render DTOs for the **active** project, in roster order.
    pub fn views(&self) -> Vec<AgentView> {
        self.order
            .iter()
            .filter(|id| self.project_of.get(id) == Some(&self.active))
            .filter_map(|id| self.agents.get(id))
            .map(AgentRecord::view)
            .collect()
    }

    /// Blocked agents **in the active project**, oldest-waiting first (drives
    /// urgent → master and triage within the current screen).
    pub fn blocked_ordered(&self) -> Vec<AgentId> {
        let mut blocked: Vec<(&AgentId, u64)> = self
            .order
            .iter()
            .filter(|id| self.project_of.get(id) == Some(&self.active))
            .filter_map(|id| {
                let rec = self.agents.get(id)?;
                rec.blocked_since.map(|since| (id, since))
            })
            .collect();
        blocked.sort_by_key(|(_, since)| *since);
        blocked.into_iter().map(|(id, _)| *id).collect()
    }

    /// The global roster order across all projects.
    pub fn order(&self) -> &[AgentId] {
        &self.order
    }

    /// The active project's roster order (what the layout engine plans over).
    pub fn active_order(&self) -> Vec<AgentId> {
        self.order
            .iter()
            .filter(|id| self.project_of.get(id) == Some(&self.active))
            .copied()
            .collect()
    }

    pub fn focus(&self) -> Option<AgentId> {
        self.focus_by_project.get(&self.active).copied()
    }

    /// Focus `id` within its own project (no-op if unknown).
    pub fn set_focus(&mut self, id: AgentId) {
        if let Some(&proj) = self.project_of.get(&id) {
            self.focus_by_project.insert(proj, id);
        }
    }

    // ---- Projects (screens) -------------------------------------------------

    /// Create a new project and return its id (does not switch to it).
    pub fn add_project(&mut self, name: impl Into<String>, cwd: PathBuf) -> ProjectId {
        let id = ProjectId(self.next_project_id);
        self.next_project_id += 1;
        self.projects.push(Project {
            id,
            name: name.into(),
            cwd,
        });
        id
    }

    /// Overwrite a project's name/cwd (used to name the default project after the
    /// real working directory). No-op if unknown.
    pub fn set_project_meta(&mut self, id: ProjectId, name: impl Into<String>, cwd: PathBuf) {
        if let Some(p) = self.projects.iter_mut().find(|p| p.id == id) {
            p.name = name.into();
            p.cwd = cwd;
        }
    }

    /// Every project (screen), in creation order.
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    /// The currently-shown project.
    pub fn active(&self) -> ProjectId {
        self.active
    }

    /// Zero-based index of the active project in [`Registry::projects`].
    pub fn active_index(&self) -> usize {
        self.projects
            .iter()
            .position(|p| p.id == self.active)
            .unwrap_or(0)
    }

    /// Switch to a project by id (no-op if unknown).
    pub fn set_active(&mut self, id: ProjectId) {
        if self.projects.iter().any(|p| p.id == id) {
            self.active = id;
        }
    }

    /// Switch to the `n`-th project (1-based, as bound to `Mod+1..9`). No-op if
    /// out of range.
    pub fn switch_to(&mut self, n: usize) {
        if let Some(p) = n.checked_sub(1).and_then(|i| self.projects.get(i)) {
            self.active = p.id;
        }
    }

    /// Cycle to the next project (screen), wrapping. The terminal-friendly way to
    /// switch when `Mod+digit` isn't encoded distinctly.
    pub fn next_project(&mut self) {
        if self.projects.is_empty() {
            return;
        }
        let next = (self.active_index() + 1) % self.projects.len();
        self.active = self.projects[next].id;
    }

    /// Every agent currently in `project`, in roster order.
    pub fn agents_in(&self, project: ProjectId) -> Vec<AgentId> {
        self.order
            .iter()
            .filter(|id| self.project_of.get(id) == Some(&project))
            .copied()
            .collect()
    }

    /// Drop every agent pane in `project` but keep the project itself — used to
    /// "close" the sole remaining screen (which can't be removed) by emptying it.
    /// Callers should terminate the agents' processes first (via the engine).
    pub fn clear_project(&mut self, project: ProjectId) {
        for aid in self.agents_in(project) {
            self.agents.remove(&aid);
            self.order.retain(|i| *i != aid);
            self.project_of.remove(&aid);
        }
        self.focus_by_project.remove(&project);
    }

    /// Remove a project and drop any agent panes still in it, switching the active
    /// project to a neighbour. Returns `false` (no-op) when it is the only project
    /// — there must always be at least one screen. Callers should terminate the
    /// agents' processes first (via the engine); this only drops their panes.
    pub fn remove_project(&mut self, id: ProjectId) -> bool {
        let Some(idx) = self.projects.iter().position(|p| p.id == id) else {
            return false;
        };
        if self.projects.len() <= 1 {
            return false;
        }
        for aid in self.agents_in(id) {
            self.agents.remove(&aid);
            self.order.retain(|i| *i != aid);
            self.project_of.remove(&aid);
        }
        self.focus_by_project.remove(&id);
        self.projects.remove(idx);
        if self.active == id {
            // Switch to the neighbour that now occupies this slot (or the last).
            let ni = idx.min(self.projects.len() - 1);
            self.active = self.projects[ni].id;
        }
        true
    }

    /// Whether any agent in `id` is blocked or sticky-urgent — drives the `!`
    /// indicator on a background project's tab (dwm urgent-tag behaviour).
    pub fn project_is_urgent(&self, id: ProjectId) -> bool {
        self.agents.iter().any(|(aid, rec)| {
            self.project_of.get(aid) == Some(&id)
                && (rec.blocked_since.is_some() || rec.meta.urgent)
        })
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

    /// Re-arm a restored pane for a freshly-attached live process: drop the stale
    /// approval/urgent state (the old process that owned it is gone) and lift a
    /// terminal state back to Idle, so the state machine stops swallowing events
    /// and the resumed process's output flows into this pane again.
    pub fn reactivate(&mut self, id: AgentId) {
        if let Some(rec) = self.agents.get_mut(&id) {
            if rec.state.is_terminal() {
                rec.state = AgentState::Idle;
            }
            rec.pending = None;
            rec.blocked_since = None;
            rec.meta.urgent = false;
            rec.streaming = false;
        }
    }

    pub fn pending_request_id(&self, id: AgentId) -> Option<String> {
        self.agents
            .get(&id)?
            .pending
            .as_ref()
            .map(|c| c.request_id.clone())
    }

    /// Move focus to the next/previous agent in the active project's roster
    /// order (wraps). No-op if the active project is empty.
    pub fn focus_step(&mut self, delta: isize) {
        let ord = self.active_order();
        if ord.is_empty() {
            return;
        }
        let cur = self
            .focus()
            .and_then(|f| ord.iter().position(|i| *i == f))
            .unwrap_or(0) as isize;
        let len = ord.len() as isize;
        let next = ((cur + delta) % len + len) % len;
        self.focus_by_project.insert(self.active, ord[next as usize]);
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

    // ---- Persistence --------------------------------------------------------

    /// Capture the whole session (projects + every agent pane) for saving to
    /// disk. Live-only state (blocked_since/pending/streaming) is intentionally
    /// dropped — a restored pane is history until re-attached to a process.
    pub fn snapshot(&self) -> crate::session::SessionState {
        let agents = self
            .order
            .iter()
            .filter_map(|id| {
                let rec = self.agents.get(id)?;
                let project_id = *self.project_of.get(id)?;
                Some(crate::session::AgentSnapshot {
                    project_id,
                    meta: rec.meta.clone(),
                    state: rec.state,
                    info: rec.info.clone(),
                    tokens: rec.tokens,
                    tail: rec.tail.iter().cloned().collect(),
                    // The Claude session id (from `init`) — the key for live resume.
                    session_id: rec.info.as_ref().and_then(|i| i.session_id.clone()),
                    is_subagent: rec.meta.name.starts_with('\u{21b3}'),
                    resumable: rec.resumable,
                    kind: rec.kind,
                })
            })
            .collect();
        crate::session::SessionState {
            version: crate::session::SCHEMA_VERSION,
            projects: self.projects.clone(),
            active: self.active,
            agents,
        }
    }

    /// Rebuild this registry from a saved [`crate::session::SessionState`],
    /// replacing all current contents. Restored agents are Idle-of-record (their
    /// saved `state`) with no live process; id/project counters are advanced past
    /// everything restored so freshly spawned agents/projects never collide.
    pub fn restore(&mut self, state: &crate::session::SessionState) {
        self.agents.clear();
        self.order.clear();
        self.project_of.clear();
        self.focus_by_project.clear();
        self.projects = state.projects.clone();
        self.active = state.active;
        self.clock = 0;
        self.next_project_id = state
            .projects
            .iter()
            .map(|p| p.id.0 + 1)
            .max()
            .unwrap_or(0);

        let mut next_id = 0u32;
        for snap in &state.agents {
            let id = snap.meta.id;
            next_id = next_id.max(id.0 + 1);
            let mut rec = Self::fresh(snap.meta.clone());
            rec.state = snap.state;
            rec.tokens = snap.tokens;
            rec.info = snap.info.clone();
            rec.tail = snap.tail.iter().cloned().collect();
            rec.resumable = snap.resumable;
            rec.kind = snap.kind;
            self.agents.insert(id, rec);
            self.order.push(id);
            self.project_of.insert(id, snap.project_id);
            self.focus_by_project.entry(snap.project_id).or_insert(id);
        }
        self.next_id = next_id;

        // Guarantee at least one project and a valid active pointer.
        if self.projects.is_empty() {
            let id = self.add_project("main", PathBuf::new());
            self.active = id;
        } else if !self.projects.iter().any(|p| p.id == self.active) {
            self.active = self.projects[0].id;
        }
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
        AgentEvent::ToolStarted { name, summary, input } => {
            let text = if summary.is_empty() {
                format!("⏺ {name}")
            } else {
                format!("⏺ {name}({summary})")
            };
            let mut out = vec![TranscriptLine::new(K::ToolCall, text)];
            if let Some(input) = input {
                out.extend(diff_lines(name, input));
            }
            out
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

/// Build red `-` / green `+` diff lines under an edit tool call, capped at
/// [`DIFF_LINES`]. Reuses existing `LineKind`s for color (`ToolError` = red for
/// removals, `ToolCall` = green for additions), so no new proto type or TUI
/// change is needed. Returns nothing when there is no textual change.
fn diff_lines(name: &str, input: &serde_json::Value) -> Vec<TranscriptLine> {
    let field = |k: &str| input.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
    // (added?, text) for each changed line, removals before additions.
    let mut signed: Vec<(bool, String)> = Vec::new();
    match name {
        // A new/overwritten file: the whole body is an addition.
        "Write" => push_change(&mut signed, "", field("content")),
        "NotebookEdit" => push_change(&mut signed, "", field("new_source")),
        "MultiEdit" => {
            if let Some(edits) = input.get("edits").and_then(serde_json::Value::as_array) {
                for e in edits {
                    let old = e.get("old_string").and_then(serde_json::Value::as_str).unwrap_or("");
                    let new = e.get("new_string").and_then(serde_json::Value::as_str).unwrap_or("");
                    push_change(&mut signed, old, new);
                }
            }
        }
        // Edit (and any other edit-family tool): old → new.
        _ => push_change(&mut signed, field("old_string"), field("new_string")),
    }

    let total = signed.len();
    let mut out: Vec<TranscriptLine> = signed
        .into_iter()
        .take(DIFF_LINES)
        .map(|(added, line)| {
            let (kind, sign) = if added {
                (LineKind::ToolCall, '+')
            } else {
                (LineKind::ToolError, '-')
            };
            TranscriptLine::new(kind, format!("  {sign} {line}"))
        })
        .collect();
    let extra = total.saturating_sub(DIFF_LINES);
    if extra > 0 {
        out.push(TranscriptLine::new(LineKind::ToolResult, format!("  … +{extra} lines")));
    }
    out
}

/// Append signed diff lines for one `old` → `new` change, trimming the common
/// leading and trailing identical lines so only the changed middle is shown.
/// Removed lines are pushed first (`false`), then added lines (`true`).
fn push_change(out: &mut Vec<(bool, String)>, old: &str, new: &str) {
    let a: Vec<&str> = if old.is_empty() { Vec::new() } else { old.lines().collect() };
    let b: Vec<&str> = if new.is_empty() { Vec::new() } else { new.lines().collect() };

    // Common prefix, then common suffix (not overlapping the prefix).
    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut end = 0;
    while end < a.len() - start
        && end < b.len() - start
        && a[a.len() - 1 - end] == b[b.len() - 1 - end]
    {
        end += 1;
    }

    for l in &a[start..a.len() - end] {
        out.push((false, (*l).to_string()));
    }
    for l in &b[start..b.len() - end] {
        out.push((true, (*l).to_string()));
    }
}

#[cfg(test)]
mod diff_tests {
    use super::*;
    use serde_json::json;

    /// Turn an `Edit`/`Write` event into its transcript lines.
    fn lines(name: &str, summary: &str, input: serde_json::Value) -> Vec<TranscriptLine> {
        transcript_lines(&AgentEvent::ToolStarted {
            name: name.into(),
            summary: summary.into(),
            input: Some(input),
        })
    }

    #[test]
    fn edit_shows_only_the_changed_middle_in_red_and_green() {
        let out = lines(
            "Edit",
            "f.rs",
            json!({ "file_path": "f.rs", "old_string": "a\nb\nc", "new_string": "a\nX\nc" }),
        );
        // Header first, unchanged surrounding lines (a, c) trimmed away.
        assert_eq!(out[0].kind, LineKind::ToolCall);
        assert_eq!(out[0].text, "⏺ Edit(f.rs)");
        let removed = out.iter().find(|l| l.kind == LineKind::ToolError).unwrap();
        assert_eq!(removed.text, "  - b");
        let added = out
            .iter()
            .find(|l| l.kind == LineKind::ToolCall && l.text.contains('+'))
            .unwrap();
        assert_eq!(added.text, "  + X");
        // Common lines `a`/`c` are trimmed: exactly one removal + one addition.
        assert_eq!(out.iter().filter(|l| l.text.starts_with("  - ")).count(), 1);
        assert_eq!(out.iter().filter(|l| l.text.starts_with("  + ")).count(), 1);
    }

    #[test]
    fn write_shows_whole_body_as_additions() {
        let out = lines("Write", "n.rs", json!({ "file_path": "n.rs", "content": "l1\nl2" }));
        let adds: Vec<_> = out
            .iter()
            .filter(|l| l.kind == LineKind::ToolCall && l.text.contains('+'))
            .collect();
        assert_eq!(adds.len(), 2);
        assert_eq!(adds[0].text, "  + l1");
        assert_eq!(adds[1].text, "  + l2");
    }

    #[test]
    fn long_diff_is_capped_with_a_tail() {
        let content: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let out = lines("Write", "big.rs", json!({ "file_path": "big.rs", "content": content }));
        let shown = out.iter().filter(|l| l.text.starts_with("  + ")).count();
        assert_eq!(shown, DIFF_LINES);
        let tail = out.last().unwrap();
        assert_eq!(tail.kind, LineKind::ToolResult);
        assert_eq!(tail.text, format!("  … +{} lines", 30 - DIFF_LINES));
    }

    #[test]
    fn multiedit_concatenates_each_change() {
        let out = lines(
            "MultiEdit",
            "f.rs",
            json!({ "edits": [
                { "old_string": "one", "new_string": "uno" },
                { "old_string": "two", "new_string": "dos" },
            ] }),
        );
        let removed: Vec<_> = out
            .iter()
            .filter(|l| l.kind == LineKind::ToolError)
            .map(|l| l.text.clone())
            .collect();
        let added: Vec<_> = out
            .iter()
            .filter(|l| l.kind == LineKind::ToolCall && l.text.contains('+'))
            .map(|l| l.text.clone())
            .collect();
        assert_eq!(removed, vec!["  - one", "  - two"]);
        assert_eq!(added, vec!["  + uno", "  + dos"]);
    }
}
