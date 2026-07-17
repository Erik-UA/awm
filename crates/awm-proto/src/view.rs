//! The render DTO — a snapshot of one agent handed to the TUI.

use crate::event::TokenUsage;
use crate::meta::AgentMeta;
use crate::state::AgentState;
use serde::{Deserialize, Serialize};

/// Everything the TUI needs to draw a single agent pane. Assembled by the core
/// from an agent's meta, current state, accounting, and recent output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentView {
    pub meta: AgentMeta,
    pub state: AgentState,
    pub tokens: TokenUsage,
    /// Most-recent output lines (the PTY ring buffer's tail), oldest first.
    pub tail: Vec<String>,
}

impl AgentView {
    /// Whether this agent should be visually highlighted as needing attention.
    #[must_use]
    pub fn is_urgent(&self) -> bool {
        self.meta.urgent || self.state.is_blocked()
    }
}
