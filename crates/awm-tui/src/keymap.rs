//! Keymap vocabulary. The [`Action`] enum is the frozen set of user intents the
//! TUI translates keys into; the core interprets them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    /// `PgUp` / `PgDn` / `Home` / `End` — scroll the focused pane's history.
    ScrollUp,
    ScrollDown,
    ScrollTop,
    ScrollBottom,
    /// `Ctrl+i` — toggle the agent inspection card (skills / plugins / tools).
    Inspect,
}

/// Translate a key press to an [`Action`], or `None` if unbound.
///
/// The window-manager bindings are all held under the CONTROL modifier
/// (`Mod` in dwm parlance); the approval bindings (`y`/`n`/`e`) are bare keys
/// acted on only when an urgent agent is focused.
#[must_use]
pub fn map_key(key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        // Mod-chorded window-manager actions.
        KeyCode::Char('j') if ctrl => Some(Action::FocusNext),
        KeyCode::Char('k') if ctrl => Some(Action::FocusPrev),
        KeyCode::Enter if ctrl => Some(Action::ZoomMaster),
        KeyCode::Char('m') if ctrl => Some(Action::ToggleMonocle),
        KeyCode::Char('p') if ctrl => Some(Action::SpawnPrompt),
        KeyCode::Char(c @ '1'..='9') if ctrl => Some(Action::ToggleTag(c as u8 - b'0')),

        // Bare approval keys (meaningful on an urgent, focused agent).
        KeyCode::Char('y') if !ctrl => Some(Action::Approve),
        KeyCode::Char('n') if !ctrl => Some(Action::Deny),
        KeyCode::Char('e') if !ctrl => Some(Action::EditInline),

        // Scrollback of the focused pane.
        KeyCode::PageUp => Some(Action::ScrollUp),
        KeyCode::PageDown => Some(Action::ScrollDown),
        KeyCode::Home => Some(Action::ScrollTop),
        KeyCode::End => Some(Action::ScrollBottom),

        // Agent inspection card.
        KeyCode::Char('i') if ctrl => Some(Action::Inspect),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn bare(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn mod_focus_keys() {
        assert_eq!(map_key(ctrl(KeyCode::Char('j'))), Some(Action::FocusNext));
        assert_eq!(map_key(ctrl(KeyCode::Char('k'))), Some(Action::FocusPrev));
    }

    #[test]
    fn mod_zoom_and_monocle() {
        assert_eq!(map_key(ctrl(KeyCode::Enter)), Some(Action::ZoomMaster));
        assert_eq!(map_key(ctrl(KeyCode::Char('m'))), Some(Action::ToggleMonocle));
    }

    #[test]
    fn mod_spawn_prompt() {
        assert_eq!(map_key(ctrl(KeyCode::Char('p'))), Some(Action::SpawnPrompt));
    }

    #[test]
    fn mod_tag_digits() {
        for n in 1..=9u8 {
            let c = (b'0' + n) as char;
            assert_eq!(
                map_key(ctrl(KeyCode::Char(c))),
                Some(Action::ToggleTag(n)),
                "Mod+{c} should toggle tag {n}"
            );
        }
    }

    #[test]
    fn bare_approval_keys() {
        assert_eq!(map_key(bare(KeyCode::Char('y'))), Some(Action::Approve));
        assert_eq!(map_key(bare(KeyCode::Char('n'))), Some(Action::Deny));
        assert_eq!(map_key(bare(KeyCode::Char('e'))), Some(Action::EditInline));
    }

    #[test]
    fn unbound_keys_return_none() {
        // A bare `j` is not a focus action (that needs Mod).
        assert_eq!(map_key(bare(KeyCode::Char('j'))), None);
        // Ctrl+y is not an approval (approvals are bare).
        assert_eq!(map_key(ctrl(KeyCode::Char('y'))), None);
        // A digit outside the tag range or an unrelated key.
        assert_eq!(map_key(bare(KeyCode::Char('0'))), None);
        assert_eq!(map_key(ctrl(KeyCode::Char('0'))), None);
        assert_eq!(map_key(bare(KeyCode::Esc)), None);
    }
}
