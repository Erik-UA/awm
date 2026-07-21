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
    /// `Mod+1..9` — switch to project (screen) `n` (1-based). NOTE: many
    /// terminals do not encode Ctrl+digit distinctly; [`Action::NextProject`] is
    /// the reliable, terminal-friendly alternative.
    SwitchProject(u8),
    /// `Mod+o` — cycle to the next project (screen), wrapping.
    NextProject,
    /// `Mod+p` — open the spawn prompt.
    SpawnPrompt,
    /// `Mod+n` — create a new project (screen).
    NewProject,
    /// `Mod+w` — close the active project (screen) and its agents.
    CloseProject,
    /// `Mod+g` — open an interactive shell console pane in the active project.
    SpawnShell,
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
    /// `Shift+Tab` — cycle the focused agent's permission mode (default →
    /// acceptEdits → plan → bypassPermissions), like claude's own mode cycling.
    CycleMode,
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
        KeyCode::Char('n') if ctrl => Some(Action::NewProject),
        KeyCode::Char('o') if ctrl => Some(Action::NextProject),
        KeyCode::Char('w') if ctrl => Some(Action::CloseProject),
        KeyCode::Char('g') if ctrl => Some(Action::SpawnShell),
        KeyCode::Char(c @ '1'..='9') if ctrl => Some(Action::SwitchProject(c as u8 - b'0')),

        // Bare approval keys (meaningful on an urgent, focused agent).
        KeyCode::Char('y') if !ctrl => Some(Action::Approve),
        KeyCode::Char('n') if !ctrl => Some(Action::Deny),
        KeyCode::Char('e') if !ctrl => Some(Action::EditInline),

        // Scrollback of the focused pane.
        KeyCode::PageUp => Some(Action::ScrollUp),
        KeyCode::PageDown => Some(Action::ScrollDown),
        KeyCode::Home => Some(Action::ScrollTop),
        KeyCode::End => Some(Action::ScrollBottom),

        // Agent inspection card. Bound to Tab (Ctrl+i and Tab are the same byte
        // 0x09 in a terminal, which crossterm reports as KeyCode::Tab).
        KeyCode::Tab => Some(Action::Inspect),

        // Shift+Tab cycles the focused agent's permission mode.
        KeyCode::BackTab => Some(Action::CycleMode),

        _ => None,
    }
}

/// The Latin letter on the same physical key as the Cyrillic character `c`, for
/// the built-in layouts (Russian ЙЦУКЕН + Ukrainian — their letter positions
/// coincide). `None` if `c` is not a covered Cyrillic letter. The result is
/// lowercase (hotkeys are lowercase Latin); uppercase input (`О`) is folded
/// first so caps/shift still resolve.
///
/// crossterm reports the layout-dependent character, not the physical key (its
/// Kitty parser discards the base-layout-key field), so this table is how a
/// Cyrillic-layout keypress is mapped back to its QWERTY hotkey. Adding another
/// layout is a few more match arms; Cyrillic/Latin codepoint ranges are
/// disjoint, so the arms compose without collisions.
#[must_use]
pub fn positional_latin(c: char) -> Option<char> {
    let lower = c.to_lowercase().next().unwrap_or(c);
    Some(match lower {
        // Russian ЙЦУКЕН → QWERTY (full a–z, so future hotkeys work too).
        'й' => 'q', 'ц' => 'w', 'у' => 'e', 'к' => 'r', 'е' => 't', 'н' => 'y',
        'г' => 'u', 'ш' => 'i', 'щ' => 'o', 'з' => 'p', 'ф' => 'a', 'ы' => 's',
        'в' => 'd', 'а' => 'f', 'п' => 'g', 'р' => 'h', 'о' => 'j', 'л' => 'k',
        'д' => 'l', 'я' => 'z', 'ч' => 'x', 'с' => 'c', 'м' => 'v', 'и' => 'b',
        'т' => 'n', 'ь' => 'm',
        // Ukrainian differs only on non-hotkey keys; і shares the `s` position
        // with Russian ы (distinct codepoints, both safe to map).
        'і' => 's',
        _ => return None,
    })
}

/// Rewrite a keypress to its QWERTY-position Latin equivalent when a Cyrillic
/// layout is active, preserving modifiers. Non-letters and Latin pass through
/// unchanged. Apply this only in command mode — text entry needs the real char.
#[must_use]
pub fn normalize_layout(mut key: KeyEvent) -> KeyEvent {
    if let KeyCode::Char(c) = key.code {
        if let Some(latin) = positional_latin(c) {
            key.code = KeyCode::Char(latin);
        }
    }
    key
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
    fn mod_digits_switch_project() {
        for n in 1..=9u8 {
            let c = (b'0' + n) as char;
            assert_eq!(
                map_key(ctrl(KeyCode::Char(c))),
                Some(Action::SwitchProject(n)),
                "Mod+{c} should switch to project {n}"
            );
        }
    }

    #[test]
    fn mod_n_new_project() {
        assert_eq!(map_key(ctrl(KeyCode::Char('n'))), Some(Action::NewProject));
    }

    #[test]
    fn mod_o_next_project() {
        assert_eq!(map_key(ctrl(KeyCode::Char('o'))), Some(Action::NextProject));
    }

    #[test]
    fn mod_w_close_project() {
        assert_eq!(map_key(ctrl(KeyCode::Char('w'))), Some(Action::CloseProject));
    }

    #[test]
    fn bare_approval_keys() {
        assert_eq!(map_key(bare(KeyCode::Char('y'))), Some(Action::Approve));
        assert_eq!(map_key(bare(KeyCode::Char('n'))), Some(Action::Deny));
        assert_eq!(map_key(bare(KeyCode::Char('e'))), Some(Action::EditInline));
    }

    #[test]
    fn tab_inspects_and_shift_tab_cycles_mode() {
        assert_eq!(map_key(bare(KeyCode::Tab)), Some(Action::Inspect));
        assert_eq!(map_key(bare(KeyCode::BackTab)), Some(Action::CycleMode));
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

    #[test]
    fn positional_latin_maps_cyrillic_by_key_position() {
        assert_eq!(positional_latin('о'), Some('j')); // physical j
        assert_eq!(positional_latin('к'), Some('r')); // physical r
        assert_eq!(positional_latin('і'), Some('s')); // Ukrainian s
        assert_eq!(positional_latin('О'), Some('j')); // uppercase folds
        // Latin and non-letters are left alone.
        assert_eq!(positional_latin('j'), None);
        assert_eq!(positional_latin('1'), None);
    }

    #[test]
    fn cyrillic_hotkeys_resolve_via_normalization() {
        // Ctrl+о (physical Ctrl+j) → focus next, once normalized.
        assert_eq!(map_key(normalize_layout(ctrl(KeyCode::Char('о')))), Some(Action::FocusNext));
        // Bare н (physical y) → approve.
        assert_eq!(map_key(normalize_layout(bare(KeyCode::Char('н')))), Some(Action::Approve));
    }

    #[test]
    fn normalization_preserves_modifiers() {
        // physical `n` is overloaded: Ctrl → NewProject, bare → Deny. The
        // modifier must survive translation so both still resolve.
        assert_eq!(map_key(normalize_layout(ctrl(KeyCode::Char('т')))), Some(Action::NewProject));
        assert_eq!(map_key(normalize_layout(bare(KeyCode::Char('т')))), Some(Action::Deny));
    }

    #[test]
    fn normalization_leaves_latin_and_nonletters_untouched() {
        assert_eq!(normalize_layout(ctrl(KeyCode::Char('j'))), ctrl(KeyCode::Char('j')));
        assert_eq!(normalize_layout(bare(KeyCode::Enter)), bare(KeyCode::Enter));
    }
}
