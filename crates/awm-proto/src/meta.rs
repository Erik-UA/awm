//! Per-agent identity and metadata carried alongside its live state.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Opaque, stable identifier for an agent within a single `awm` session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentId(pub u32);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "@{}", self.0)
    }
}

bitflags! {
    /// User-assignable tags, one per `Mod+1..9` keybinding. An agent may hold
    /// several at once; layouts filter/group by tag.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Tags: u16 {
        const TAG1 = 1 << 0;
        const TAG2 = 1 << 1;
        const TAG3 = 1 << 2;
        const TAG4 = 1 << 3;
        const TAG5 = 1 << 4;
        const TAG6 = 1 << 5;
        const TAG7 = 1 << 6;
        const TAG8 = 1 << 7;
        const TAG9 = 1 << 8;
    }
}

impl Tags {
    /// The tag bit for a 1-based slot (`Mod+1` → `slot 1`). Slots outside 1..=9
    /// yield the empty set, so callers never index out of range.
    #[must_use]
    pub fn slot(n: u8) -> Tags {
        if (1..=9).contains(&n) {
            Tags::from_bits_truncate(1 << (n - 1))
        } else {
            Tags::empty()
        }
    }
}

/// Identity and bookkeeping for an agent, independent of its live state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMeta {
    pub id: AgentId,
    /// Human-facing label shown in the status bar.
    pub name: String,
    pub tags: Tags,
    /// Working directory the session was spawned in.
    pub cwd: PathBuf,
    /// Spawn time as unix epoch milliseconds (serde-friendly; avoids `Instant`).
    pub started_at: u64,
    /// Sticky urgent flag — set when the agent blocks on approval, cleared when
    /// the human resolves it. Drives the urgent → master promotion.
    pub urgent: bool,
}

impl AgentMeta {
    /// A fresh, untagged, non-urgent agent.
    #[must_use]
    pub fn new(id: AgentId, name: impl Into<String>, cwd: PathBuf, started_at: u64) -> Self {
        AgentMeta {
            id,
            name: name.into(),
            tags: Tags::empty(),
            cwd,
            started_at,
            urgent: false,
        }
    }
}
