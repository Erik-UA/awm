//! Normalized agent events.
//!
//! `AgentEvent` is the wire-format-independent vocabulary the rest of `awm`
//! speaks. The parser (Track B) translates raw stream-json into these; the core
//! (Phase 3) drives the state machine and layout from them. Because the killer
//! feature (urgent → master on approval) depends only on
//! [`AgentEvent::ApprovalRequested`], the exact stream-json shape can change
//! without touching this enum — the parser absorbs that behind
//! [`crate::EventSource`].

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A single normalized event emitted by an agent session.
///
/// Feed these to [`crate::AgentState::apply`] to advance an agent's lifecycle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Session initialized (maps from the stream-json `system/init` event).
    Started { model: String, cwd: PathBuf },
    /// The agent produced assistant text — it is actively working.
    Thinking,
    /// The agent began invoking a tool.
    ToolStarted { name: String },
    /// The agent is blocked awaiting approval for a tool call. This is the
    /// trigger for the urgent → master layout behavior.
    ApprovalRequested(ApprovalCtx),
    /// A pending approval was answered (approved or denied).
    ApprovalResolved { approved: bool },
    /// Token accounting update.
    Tokens(TokenUsage),
    /// The session ended. `ok` distinguishes success from failure.
    Finished { ok: bool },
    /// An unrecognized or malformed input line. Carries no state transition —
    /// the parser emits this instead of panicking on garbage.
    Noise,
}

/// Context captured when an agent blocks on an approval gate.
///
/// Populated from the SDK control channel's `can_use_tool` control_request
/// (ground-truthed against Claude Code 2.1.212 — see `docs/approval-findings.md`).
/// `request_id` correlates the block with its eventual `control_response` so the
/// controller can answer this exact request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalCtx {
    /// The tool the agent wants to run (`tool_name`, e.g. `"Bash"`, `"Write"`).
    pub tool: String,
    /// The tool's proposed input (`input`), kept opaque so new tools need no
    /// proto change.
    pub input: serde_json::Value,
    /// Correlation id for answering this exact request via `control_response`.
    pub request_id: String,
    /// The tool_use id from the passive stream, to correlate the gate with the
    /// `ToolStarted` it belongs to. Absent on older/partial wire versions.
    pub tool_use_id: Option<String>,
    /// Short human summary the CLI provides (`description`, e.g. a target path).
    pub description: Option<String>,
    /// Why the CLI decided this needs approval (`decision_reason`, e.g.
    /// "Path is outside allowed working directories").
    pub decision_reason: Option<String>,
    /// A unified diff for edit-style tools, when awm derives one for display.
    /// Not present on the wire — populated by the core, not the control channel.
    pub diff: Option<String>,
}

/// Cumulative token usage for a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

impl TokenUsage {
    /// Total tokens billed (input + output), for the status bar.
    pub fn total(&self) -> u64 {
        self.input.saturating_add(self.output)
    }
}
