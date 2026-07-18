//! The render DTO — a snapshot of one agent handed to the TUI.

use crate::event::TokenUsage;
use crate::meta::AgentMeta;
use crate::state::AgentState;
use serde::{Deserialize, Serialize};

/// The role of a transcript line, so the TUI can style it like Claude Code
/// (glyph + color) without re-parsing the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    /// Session/system notices (`● session started`).
    System,
    /// Assistant output text (rendered as markdown).
    Text,
    /// Internal reasoning (`✻ …`, dimmed).
    Thinking,
    /// A tool invocation (`⏺ Tool(args)`).
    ToolCall,
    /// A tool's output line (`⎿ …`, dimmed).
    ToolResult,
    /// A tool's output that errored (`⎿ …`, red).
    ToolError,
    /// Out-of-band note (the user's own dialogue line, approvals).
    Note,
    /// An approval prompt (urgent styling).
    Approval,
}

/// One line in an agent's window transcript.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub kind: LineKind,
    pub text: String,
}

impl TranscriptLine {
    pub fn new(kind: LineKind, text: impl Into<String>) -> Self {
        TranscriptLine {
            kind,
            text: text.into(),
        }
    }
}

/// Everything the TUI needs to draw a single agent pane. Assembled by the core
/// from an agent's meta, current state, accounting, and recent output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentView {
    pub meta: AgentMeta,
    pub state: AgentState,
    pub tokens: TokenUsage,
    /// Most-recent transcript lines (the window body), oldest first.
    pub tail: Vec<TranscriptLine>,
}

impl AgentView {
    /// Whether this agent should be visually highlighted as needing attention.
    #[must_use]
    pub fn is_urgent(&self) -> bool {
        self.meta.urgent || self.state.is_blocked()
    }
}
