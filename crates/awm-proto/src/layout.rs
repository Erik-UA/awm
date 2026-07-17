//! Layout commands — the language the core's layout engine speaks to the TUI.
//!
//! The TUI is a pure function of the current `Vec<AgentView>` plus the active
//! [`LayoutCmd`]; it never decides layout itself. This keeps the dwm-style
//! dynamic behavior (urgent → master, triage) entirely in the core.

use crate::meta::AgentId;
use serde::{Deserialize, Serialize};

/// A rendering directive for the TUI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum LayoutCmd {
    /// Put one agent in the large master zone.
    SetMaster(AgentId),
    /// Arrange the given agents as the side stack, in order.
    Stack(Vec<AgentId>),
    /// Full-screen a single agent (zoom).
    Monocle(AgentId),
    /// Show only blocked agents, ordered oldest-wait-first, for approval triage.
    Triage(Vec<AgentId>),
    /// Move keyboard focus to an agent without changing the arrangement.
    Focus(AgentId),
}
