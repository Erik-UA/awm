//! The layout engine — a pure function from registry state to a [`LayoutCmd`].
//!
//! This is where the killer feature lives: in the default tiling mode, whenever
//! any agent is blocked on approval, the oldest-waiting blocked agent is
//! promoted to the master zone (**urgent → master**).

use crate::registry::Registry;
use awm_proto::LayoutCmd;

/// The active arrangement style. Toggled by the user; the engine still applies
/// urgent → master within [`LayoutMode::Tiling`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutMode {
    /// Master + side stack (the default).
    #[default]
    Tiling,
    /// Full-screen the focused agent.
    Monocle,
    /// Only blocked agents, oldest-waiting first (approval triage).
    Triage,
}

/// Compute the layout command for the current registry + mode.
pub fn plan_layout(reg: &Registry, mode: LayoutMode) -> LayoutCmd {
    let blocked = reg.blocked_ordered();

    match mode {
        LayoutMode::Monocle => match reg.focus().or_else(|| reg.active_order().first().copied()) {
            Some(id) => LayoutCmd::Monocle(id),
            None => LayoutCmd::Stack(Vec::new()),
        },
        // Triage focuses the approval backlog; with nothing blocked it is
        // pointless, so fall back to the tiling arrangement.
        LayoutMode::Triage if !blocked.is_empty() => LayoutCmd::Triage(blocked),
        LayoutMode::Triage | LayoutMode::Tiling => {
            // urgent → master: oldest blocked wins the master zone, else focus,
            // else the first agent.
            let master = blocked
                .first()
                .copied()
                .or_else(|| reg.focus())
                .or_else(|| reg.active_order().first().copied());
            match master {
                Some(id) => LayoutCmd::SetMaster(id),
                None => LayoutCmd::Stack(Vec::new()),
            }
        }
    }
}
