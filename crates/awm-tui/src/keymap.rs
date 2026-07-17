//! Keymap vocabulary. The [`Action`] enum is the frozen set of user intents the
//! TUI translates keys into; the core interprets them. `map_key` is stubbed for
//! Phase 2.

use crossterm::event::KeyEvent;

/// A user intent produced from a key press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `Mod+j` / `Mod+k` — move focus down/up the stack.
    FocusNext,
    FocusPrev,
    /// `Mod+Enter` — zoom the focused agent into the master zone.
    ZoomMaster,
    /// `Mod+m` — toggle monocle (full-screen) layout.
    ToggleMonocle,
    /// `Mod+1..9` — toggle tag `n` (1-based) on the focused agent.
    ToggleTag(u8),
    /// `Mod+p` — open the spawn prompt.
    SpawnPrompt,
    /// `y` / `n` on an urgent agent — approve / deny the pending request.
    Approve,
    Deny,
    /// `e` on an urgent agent — zoom in to answer manually.
    EditInline,
}

/// Translate a key press to an [`Action`], or `None` if unbound. **Stub.**
pub fn map_key(_key: KeyEvent) -> Option<Action> {
    unimplemented!("Track C / Phase 2")
}
