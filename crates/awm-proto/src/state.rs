//! The agent lifecycle state machine — the single source of truth.
//!
//! ```text
//! Idle ──► Working ──► BlockedOnApproval ──► Working ──► Done
//!             │               (urgent set here)  │  └──────► Failed
//!             └───────────────────────────────────┘
//! ```
//!
//! [`AgentState::apply`] is a **total** function: every `(state, event)` pair
//! has a defined result, unexpected pairs are inert, and the terminal states
//! ([`AgentState::Done`] / [`AgentState::Failed`]) are absorbing. This makes the
//! machine safe to drive from untrusted/garbled event streams.

use crate::event::AgentEvent;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a single agent session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Spawned but not yet producing output.
    #[default]
    Idle,
    /// Actively thinking or running tools.
    Working,
    /// Halted at an approval gate. The core promotes these agents to the master
    /// zone and flags them urgent.
    BlockedOnApproval,
    /// Finished successfully. Absorbing.
    Done,
    /// Finished with an error. Absorbing.
    Failed,
}

impl AgentState {
    /// Advance the state by one event. Total and side-effect-free.
    ///
    /// Terminal states are absorbing; [`AgentEvent::Noise`] and
    /// [`AgentEvent::Tokens`] never change state.
    #[must_use]
    pub fn apply(self, event: &AgentEvent) -> AgentState {
        use AgentEvent as E;
        use AgentState as S;

        // Terminal states swallow everything.
        if self.is_terminal() {
            return self;
        }

        match event {
            E::Started { .. } => S::Working,
            // Info is metadata only — no lifecycle change.
            E::Info(_) => self,
            E::Thinking { .. }
            | E::Message { .. }
            | E::MessageDelta { .. }
            | E::ToolStarted { .. }
            | E::ToolResult { .. } => S::Working,
            E::ApprovalRequested(_) => S::BlockedOnApproval,
            // Whether approved or denied, the agent resumes working (it either
            // proceeds or handles the denial before finishing).
            E::ApprovalResolved { .. } => S::Working,
            // A turn finished but the session lives on — back to Idle (ready).
            E::TurnEnded { .. } => S::Idle,
            E::Finished { ok: true } => S::Done,
            E::Finished { ok: false } => S::Failed,
            // Pure accounting / unrecognized input: no transition.
            E::Tokens(_) | E::Noise => self,
        }
    }

    /// Whether this is an absorbing end state.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, AgentState::Done | AgentState::Failed)
    }

    /// Whether the agent is waiting on a human — the urgent condition.
    #[must_use]
    pub fn is_blocked(self) -> bool {
        matches!(self, AgentState::BlockedOnApproval)
    }
}
