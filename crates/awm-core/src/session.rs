//! Session persistence — a serializable snapshot of every project (screen) and
//! agent pane, so an `awm` session survives a restart.
//!
//! [`SessionState`] is pure data (serde). The core builds one from a
//! [`crate::Registry`] via [`crate::Registry::snapshot`] and rebuilds a registry
//! from one via [`crate::Registry::restore`]. Live agent processes cannot be
//! serialized — a snapshot captures each pane's *history* (state, transcript,
//! metadata) plus its `session_id`, which the runtime later uses to bring the
//! agent back live (`claude --resume <session_id>`).

use crate::registry::{Project, ProjectId};
use awm_proto::{AgentInfo, AgentMeta, AgentState, TokenUsage, TranscriptLine};
use serde::{Deserialize, Serialize};

/// Bump when the on-disk shape changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

/// A single agent pane, frozen for persistence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    /// Which project (screen) this pane belongs to.
    pub project_id: ProjectId,
    pub meta: AgentMeta,
    pub state: AgentState,
    pub info: Option<AgentInfo>,
    pub tokens: TokenUsage,
    /// The pane's transcript body (oldest first).
    pub tail: Vec<TranscriptLine>,
    /// The agent's Claude session id, when known — the key for live resume.
    /// `None` for mocks and until the parser reports it.
    #[serde(default)]
    pub session_id: Option<String>,
    /// A spawned sub-agent pane (no process of its own; not independently
    /// resumable). Restored as history only.
    #[serde(default)]
    pub is_subagent: bool,
    /// Whether this is a live-resumable Claude session (vs a mock). Only these
    /// are re-attached via `claude --resume` on restore.
    #[serde(default)]
    pub resumable: bool,
}

/// A whole `awm` session: its projects, the active one, and every agent pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub version: u32,
    pub projects: Vec<Project>,
    pub active: ProjectId,
    pub agents: Vec<AgentSnapshot>,
}
