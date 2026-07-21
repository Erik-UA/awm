//! Track C — Ratatui rendering and keymap.
//!
//! [`AwmTui`] wraps a ratatui [`Terminal`] over any backend, so tests can drive
//! it with `TestBackend` for snapshotting. [`AwmTui::render`] is a *pure
//! function* of the agent views plus the active [`LayoutCmd`] — it owns no
//! layout policy of its own (the core decides urgent → master promotion, etc.).

#![forbid(unsafe_code)]

use std::collections::HashMap;

use awm_proto::{AgentId, AgentState, AgentView, LayoutCmd, LineKind, Renderer, Tags, TranscriptLine};
use awm_pty::{ShellColor, ShellScreen};
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

pub mod keymap;

/// One project (screen) tab shown in the top bar. Built by the binary from the
/// core's `Registry::projects()` + `active`/`project_is_urgent`; kept here (not in
/// the frozen `awm-proto`) so the TUI stays self-contained and doesn't depend on
/// `awm-core`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub name: String,
    /// Whether this is the currently-shown project.
    pub active: bool,
    /// Whether any agent on this (possibly background) project is blocked/urgent.
    pub urgent: bool,
}

/// A snapshot of the directory browser (`Ctrl+n`) for rendering. Built by the
/// binary from its `Picker` state; kept here (not in proto) like [`Tab`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerView {
    /// The directory currently being browsed (shown as the title).
    pub path: String,
    /// Row labels: `../` first (unless at root), then `name/` per matching
    /// subdirectory (already filtered by `query`).
    pub entries: Vec<String>,
    /// Index of the highlighted row in `entries`.
    pub selected: usize,
    /// The active prefix filter (empty = no filter). Shown in the title.
    pub query: String,
}

/// One selectable option in a [`GateView`]. `checked` only matters for
/// multi-select gates; single-select ignores it and reports [`GateView::selected`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateOption {
    pub label: String,
    pub checked: bool,
}

impl GateOption {
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        GateOption { label: label.into(), checked: false }
    }
}

/// One question group in a [`GateView`]: a header/prompt over a set of options.
/// `AskUserQuestion` maps each of its questions to a group; plain gates use a
/// single group. Not `Eq` (owns owned strings only — but kept `PartialEq`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateGroup {
    /// Short header (AskUserQuestion `header`; empty for plan/tool gates).
    pub header: String,
    /// The question / prompt text shown above the options.
    pub prompt: String,
    /// The choices, in display order.
    pub options: Vec<GateOption>,
    /// The chosen row for a single-select group (radio pick); ignored when `multi`.
    pub selected: usize,
    /// Multi-select (checkboxes) vs single-select (radio).
    pub multi: bool,
}

/// An interactive decision overlay: an optional markdown `body` (e.g. an
/// `ExitPlanMode` plan) above one or more keyboard-navigable question `groups`.
/// Built by the binary from an `ApprovalCtx`; kept here (not in the frozen
/// `awm-proto`) like [`PickerView`] and [`Tab`]. Mirrors Claude Code's approval /
/// plan prompt and the multi-question `AskUserQuestion` widget.
///
/// A flat cursor `(cursor_group, cursor_opt)` walks across all groups; `Space`
/// picks a radio / toggles a checkbox; `Enter` submits every group.
///
/// Not `Eq` because `TranscriptLine` (in the frozen `awm-proto`) is only
/// `PartialEq`.
#[derive(Clone, Debug, PartialEq)]
pub struct GateView {
    /// Panel title (e.g. the tool name, or `"Ready to code?"`).
    pub title: String,
    /// Markdown body shown above the groups (empty = an options-only gate).
    pub body: Vec<TranscriptLine>,
    /// One or more question groups (≥1; plain gates use a single Yes/No group).
    pub groups: Vec<GateGroup>,
    /// The active group + option under the cursor.
    pub cursor_group: usize,
    pub cursor_opt: usize,
}

impl GateView {
    /// Move the flat cursor by `delta` across the concatenation of every group's
    /// options (crossing group boundaries), clamped (no wrap-around).
    pub fn move_cursor(&mut self, delta: isize) {
        // Flatten to a global index, shift, clamp, then map back to (group, opt).
        let flat: Vec<(usize, usize)> = self
            .groups
            .iter()
            .enumerate()
            .flat_map(|(gi, g)| (0..g.options.len()).map(move |oi| (gi, oi)))
            .collect();
        if flat.is_empty() {
            return;
        }
        let cur = flat
            .iter()
            .position(|&(gi, oi)| gi == self.cursor_group && oi == self.cursor_opt)
            .unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, flat.len() as isize - 1) as usize;
        let (gi, oi) = flat[next];
        self.cursor_group = gi;
        self.cursor_opt = oi;
    }

    /// Pick/toggle the option under the cursor: multi-select flips its `checked`
    /// flag; single-select sets the group's `selected` (radio).
    pub fn toggle(&mut self) {
        if let Some(g) = self.groups.get_mut(self.cursor_group) {
            if g.multi {
                if let Some(o) = g.options.get_mut(self.cursor_opt) {
                    o.checked = !o.checked;
                }
            } else {
                g.selected = self.cursor_opt;
            }
        }
    }

    /// The chosen option indices per group, submitted on Enter: single-select →
    /// `[selected]`, multi-select → every checked option.
    #[must_use]
    pub fn chosen(&self) -> Vec<Vec<usize>> {
        self.groups
            .iter()
            .map(|g| {
                if g.multi {
                    g.options
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| o.checked)
                        .map(|(i, _)| i)
                        .collect()
                } else {
                    vec![g.selected]
                }
            })
            .collect()
    }
}

/// Per-frame animation/timing context for the live status indicator: `tick`
/// advances the spinner phase, `elapsed` maps an agent to the seconds it has
/// spent in the current active turn. Cheap to copy (holds a shared ref).
#[derive(Clone, Copy)]
struct Chrome<'a> {
    tick: u64,
    elapsed: &'a HashMap<AgentId, u64>,
    /// Live terminal grids for shell panes, keyed by pane id. A pane present here
    /// renders its emulator grid instead of an agent transcript.
    shells: &'a HashMap<AgentId, ShellScreen>,
    /// Where `draw_pane` records each shell pane's inner `(rows, cols)` so the
    /// binary can resize the matching PTY to fit.
    shell_sizes: &'a std::cell::RefCell<HashMap<AgentId, (u16, u16)>>,
}

/// The TUI renderer, generic over a ratatui backend.
pub struct AwmTui<B: Backend> {
    terminal: Terminal<B>,
    /// Spinner phase, advanced by the caller each frame (0 in tests → static).
    tick: u64,
    /// Per-agent seconds spent in the current active turn (empty in tests).
    elapsed: HashMap<AgentId, u64>,
    /// Inner `(rows, cols)` each shell pane was drawn at this frame, so the
    /// binary can resize the matching PTY. Filled during `draw`, read after.
    shell_sizes: std::cell::RefCell<HashMap<AgentId, (u16, u16)>>,
}

impl<B: Backend> AwmTui<B> {
    /// Build a TUI over `backend` (e.g. `TestBackend` in tests, a crossterm
    /// backend in the real app).
    pub fn new(backend: B) -> std::io::Result<Self> {
        Ok(AwmTui {
            terminal: Terminal::new(backend)?,
            tick: 0,
            elapsed: HashMap::new(),
            shell_sizes: std::cell::RefCell::new(HashMap::new()),
        })
    }

    /// The inner `(rows, cols)` each shell pane occupied in the last `draw`.
    /// The binary resizes each shell's PTY to match so full-screen apps line up.
    pub fn shell_sizes(&self) -> HashMap<AgentId, (u16, u16)> {
        self.shell_sizes.borrow().clone()
    }

    /// Feed the live-status animation/timing for the next `draw`: `tick` is a
    /// monotonically increasing frame counter (spinner phase); `elapsed` maps an
    /// agent to seconds in its current active turn. Kept off `draw`'s signature
    /// so existing callers/tests are unaffected (they render a static frame 0).
    pub fn set_chrome(&mut self, tick: u64, elapsed: HashMap<AgentId, u64>) {
        self.tick = tick;
        self.elapsed = elapsed;
    }

    /// Borrow the underlying backend — lets tests snapshot a `TestBackend`.
    pub fn backend(&self) -> &B {
        self.terminal.backend()
    }
}

impl<B: Backend> AwmTui<B> {
    /// Draw a frame with an optional focus highlight and an optional bottom
    /// input bar (the `Ctrl+p` spawn prompt). `render` from the [`Renderer`]
    /// trait is this with both `None`.
    /// Draw a frame. `scroll` is the focused pane's scrollback offset (0 = follow
    /// the newest output); `show_card` renders the focused agent's inspection card
    /// instead of the normal layout. `render` from [`Renderer`] passes the defaults.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        views: &[AgentView],
        layout: &LayoutCmd,
        focus: Option<AgentId>,
        prompt: Option<&str>,
        scroll: u16,
        show_card: bool,
        show_help: bool,
        picker: Option<&PickerView>,
        gate: Option<&GateView>,
        gate_target: Option<AgentId>,
        tabs: &[Tab],
        shells: &HashMap<AgentId, ShellScreen>,
    ) -> std::io::Result<()> {
        // Fresh per-frame; draw_pane refills it for whatever shells are visible.
        self.shell_sizes.borrow_mut().clear();
        // Borrow the timing fields disjointly from `self.terminal` so the draw
        // closure can carry them without capturing all of `self`.
        let chrome = Chrome {
            tick: self.tick,
            elapsed: &self.elapsed,
            shells,
            shell_sizes: &self.shell_sizes,
        };
        self.terminal.draw(|frame| {
            let full = frame.size();
            // Reserve a one-row project tab bar at the very top when there are
            // projects to show; with no tabs the layout is byte-identical to
            // before (so `render()`/snapshots are unaffected).
            let body = if tabs.is_empty() {
                full
            } else {
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Min(1)])
                    .split(full);
                draw_tab_bar(frame, tabs, rows[0]);
                rows[1]
            };
            let area = match prompt {
                Some(text) => {
                    let rows = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(1), Constraint::Length(1)])
                        .split(body);
                    draw_prompt_bar(frame, text, rows[1]);
                    rows[0]
                }
                None => body,
            };
            // The directory browser takes over the whole body (highest precedence).
            if let Some(pv) = picker {
                render_picker(frame, pv, area);
                return;
            }
            // A demo gate (`g`/`G`, no live target) takes over the body as a
            // full-screen overlay. A LIVE gate (`gate_target` set) instead renders
            // inline at the bottom of its agent's pane — see `render_into`.
            if let (Some(gv), None) = (gate, gate_target) {
                render_gate(frame, gv, area);
                return;
            }
            // The help overlay takes over the whole body.
            if show_help {
                render_help(frame, area);
                return;
            }
            // The inspection card takes over the focused agent's view.
            if show_card {
                if let Some(v) = focus.and_then(|id| find(views, id)) {
                    render_card(frame, v, area);
                    return;
                }
            }
            // A live gate belongs to a specific agent's pane (inline menu).
            let inline = match (gate, gate_target) {
                (Some(g), Some(t)) => Some((g, t)),
                _ => None,
            };
            render_into(frame, views, layout, focus, scroll, inline, area, chrome);
        })?;
        Ok(())
    }
}

impl<B: Backend> Renderer for AwmTui<B> {
    fn render(&mut self, views: &[AgentView], layout: &LayoutCmd) -> std::io::Result<()> {
        self.draw(
            views,
            layout,
            None,
            None,
            0,
            false,
            false,
            None,
            None,
            None,
            &[],
            &HashMap::new(),
        )
    }
}

/// The directory browser overlay (`Ctrl+n`): a bordered panel titled with the
/// current path, a scrolling list of `../` + subdirectories with the highlighted
/// row reversed, and a footer legend of the keys. The viewport follows the
/// selection so it is always visible in long directories.
fn render_picker(frame: &mut Frame, view: &PickerView, area: Rect) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let path = elide_left(&view.path, inner_w.saturating_sub(2));
    let title = if view.query.is_empty() {
        format!(" {path} ")
    } else {
        format!(" {path} — find: {} ", view.query)
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(&block, area);
    let inner = block.inner(area);
    if inner.height == 0 {
        return;
    }

    // Reserve the last inner row for the key legend.
    let list_h = inner.height.saturating_sub(1).max(1) as usize;
    // Scroll so `selected` stays inside the visible window.
    let start = view.selected.saturating_sub(list_h.saturating_sub(1)).min(
        view.entries.len().saturating_sub(list_h),
    );
    let mut lines: Vec<Line> = Vec::new();
    for (i, label) in view.entries.iter().enumerate().skip(start).take(list_h) {
        let style = if i == view.selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else if label == "../" {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Gray)
        };
        let marker = if i == view.selected { "> " } else { "  " };
        lines.push(Line::from(Span::styled(format!("{marker}{label}"), style)));
    }
    // Pad to fill, then the legend on the last row.
    while lines.len() < list_h {
        lines.push(Line::from(String::new()));
    }
    lines.push(Line::from(Span::styled(
        "type: filter · ↑↓ move · Enter/→ open · ← up · Tab select · ⌫ del/up · Esc clear",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render every question group to styled lines, marking the option under the
/// flat cursor with `❯`. Single-select shows a `(●)`/`( )` radio; multi-select
/// shows `[x]`/`[ ]`. Returns the lines plus the cursor row's line index (so an
/// inline footer can scroll to keep it visible). A blank line separates groups.
fn gate_group_lines(view: &GateView) -> (Vec<Line<'static>>, usize) {
    let hdr = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cursor_line = 0usize;
    for (gi, g) in view.groups.iter().enumerate() {
        if gi > 0 {
            out.push(Line::from(String::new())); // gap between groups
        }
        if !g.header.is_empty() {
            out.push(Line::from(Span::styled(g.header.clone(), hdr)));
        }
        if !g.prompt.is_empty() {
            out.push(Line::from(Span::styled(
                g.prompt.clone(),
                Style::default().fg(Color::Gray),
            )));
        }
        for (oi, opt) in g.options.iter().enumerate() {
            let at_cursor = gi == view.cursor_group && oi == view.cursor_opt;
            if at_cursor {
                cursor_line = out.len();
            }
            let cur = if at_cursor { "\u{276f} " } else { "  " };
            let mark = if g.multi {
                if opt.checked { "[x] " } else { "[ ] " }
            } else if g.selected == oi {
                "(\u{25cf}) " // filled radio = the picked option
            } else {
                "( ) "
            };
            let style = if at_cursor {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            out.push(Line::from(Span::styled(
                format!("{cur}{mark}{}", opt.label),
                style,
            )));
        }
    }
    (out, cursor_line)
}

/// The key hint shown under a gate. `cancel` picks the closing verb
/// (overlay = "cancel", inline = "hide").
fn gate_hint(view: &GateView, cancel: &str) -> String {
    let multi = view.groups.iter().any(|g| g.multi);
    let pick = if multi { "Space toggle" } else { "Space pick" };
    format!("\u{2191}\u{2193} move \u{b7} {pick} \u{b7} Enter send \u{b7} Esc {cancel}")
}

/// The interactive decision overlay (approval gate / `ExitPlanMode` plan): a
/// bordered panel with an optional markdown body over the question group(s) and a
/// key legend. Mirrors `render_picker`'s look. (Used by the `g`/`G`-style demo
/// snapshots; live gates render inline — see `gate_inline_lines`.)
fn render_gate(frame: &mut Frame, view: &GateView, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", view.title));
    frame.render_widget(&block, area);
    let inner = block.inner(area);
    if inner.height == 0 {
        return;
    }
    let width = inner.width as usize;

    let (group_lines, _) = gate_group_lines(view);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),                          // markdown body
            Constraint::Length(group_lines.len() as u16), // groups
            Constraint::Length(1),                       // legend
        ])
        .split(inner);

    if !view.body.is_empty() && rows[0].height > 0 {
        let mut body: Vec<Line> = Vec::new();
        for tl in &view.body {
            body.extend(render_transcript_line(tl, width));
        }
        body.truncate(rows[0].height as usize);
        frame.render_widget(Paragraph::new(body), rows[0]);
    }

    frame.render_widget(Paragraph::new(group_lines), rows[1]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            gate_hint(view, "cancel"),
            Style::default().fg(Color::DarkGray),
        ))),
        rows[2],
    );
}

/// Left-elide a path to at most `w` columns, prefixing `…` when truncated.
fn elide_left(s: &str, w: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= w || w == 0 {
        return s.to_string();
    }
    let tail: String = chars[chars.len() - (w - 1)..].iter().collect();
    format!("…{tail}")
}

/// The keybinding help overlay (toggled by `?`). A bordered panel grouping the
/// bindings by area. Kept in sync with `keymap::map_key` and the binary's direct
/// keys (`i`/`r`/`q`, Ctrl+x/t).
fn render_help(frame: &mut Frame, area: Rect) {
    let key = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let head = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);

    let mut body: Vec<Line> = Vec::new();
    let section = |body: &mut Vec<Line>, title: &str, rows: &[(&str, &str)]| {
        body.push(Line::from(Span::styled(title.to_string(), head)));
        for (k, desc) in rows {
            body.push(Line::from(vec![
                Span::styled(format!("  {k:<12}"), key),
                Span::raw((*desc).to_string()),
            ]));
        }
        body.push(Line::from(String::new()));
    };

    section(
        &mut body,
        "Screens (projects)",
        &[
            ("Ctrl+n", "new project (folder browser)"),
            ("Ctrl+w", "close active project (empties the last one)"),
            ("Ctrl+o", "next project (wraps)"),
            ("Ctrl+1..9", "switch to project N (if terminal sends it)"),
        ],
    );
    section(
        &mut body,
        "Agents",
        &[
            ("Ctrl+p", "spawn an agent on this screen"),
            ("Ctrl+g", "open a shell console on this screen"),
            ("i", "message the focused agent"),
            ("r", "resume a restored (dead) claude pane"),
            ("Ctrl+x", "kill the focused agent / shell"),
        ],
    );
    section(
        &mut body,
        "Shell console",
        &[
            ("(focused)", "keys type straight into the shell"),
            ("Ctrl+b", "prefix: next key is an awm hotkey (e.g. Ctrl+b k)"),
        ],
    );
    section(
        &mut body,
        "Focus & layout",
        &[
            ("Ctrl+j / k", "focus next / previous pane"),
            ("Ctrl+Enter", "zoom focused pane to master"),
            ("Ctrl+m", "toggle monocle (full-screen)"),
            ("Ctrl+t", "toggle approval triage"),
            ("Tab", "toggle the agent inspect card"),
            ("Shift+Tab", "cycle permission mode"),
            ("PgUp/PgDn", "scroll (Home/End = top/bottom)"),
        ],
    );
    section(
        &mut body,
        "Approvals & app",
        &[
            ("↑↓ / Space", "move / toggle in the inline decision menu"),
            ("Enter", "send the menu choice to the agent"),
            ("y / n", "approve / deny (oldest blocked agent)"),
            ("e / Esc", "reopen / hide the inline decision menu"),
            ("?", "toggle this help"),
            ("q", "quit (saves the session)"),
        ],
    );
    body.push(Line::from(Span::styled(
        "press ? or Esc to close",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" awm — keybindings ");
    frame.render_widget(Paragraph::new(body).block(block), area);
}

/// The top project tab bar: `[1:awm] [2:web !] [3:docs]`. The active project is
/// bold + reversed; any project with a blocked/urgent agent shows a red `!` (so
/// the cross-screen urgent signal is visible even in a symbol-only snapshot).
fn draw_tab_bar(frame: &mut Frame, tabs: &[Tab], area: Rect) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let mark = if tab.urgent { " !" } else { "" };
        let label = format!("[{}:{}{}]", i + 1, tab.name, mark);
        let mut style = if tab.active {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Gray)
        };
        if tab.urgent {
            style = style.fg(Color::Red).add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Dispatch a layout command into `area`, optionally marking the focused agent.
/// `scroll` applies to whichever pane is focused.
fn render_into(
    frame: &mut Frame,
    views: &[AgentView],
    layout: &LayoutCmd,
    focus: Option<AgentId>,
    scroll: u16,
    inline: Option<(&GateView, AgentId)>,
    area: Rect,
    chrome: Chrome,
) {
    match layout {
        LayoutCmd::Monocle(id) => match find(views, *id) {
            Some(v) => draw_pane(frame, v, focus == Some(v.meta.id), scroll, inline, area, chrome),
            None => draw_empty(frame, area),
        },
        LayoutCmd::Triage(ids) => {
            draw_triage(frame, views, ids, focus, scroll, inline, area, chrome)
        }
        // Both promote a single agent to the master zone; the rest of the roster
        // falls into the side stack in roster order.
        LayoutCmd::SetMaster(id) | LayoutCmd::Focus(id) => {
            let master = *id;
            let stack: Vec<AgentId> = views
                .iter()
                .map(|v| v.meta.id)
                .filter(|i| *i != master)
                .collect();
            draw_master_stack(frame, views, master, &stack, focus, scroll, inline, area, chrome);
        }
        // Treat the head of the stack as master, the tail as the stack.
        LayoutCmd::Stack(ids) => match ids.split_first() {
            Some((first, rest)) => {
                draw_master_stack(frame, views, *first, rest, focus, scroll, inline, area, chrome)
            }
            None => draw_empty(frame, area),
        },
    }
}

/// The agent inspection card (model / mode / tools / skills / …), parsed from
/// the `init` metadata. Bordered panel titled with the agent name; body shows
/// the model and permission mode, then one labelled section per capability list
/// (tools / skills / plugins / slash-commands / subagents), each wrapped to fit.
fn render_card(frame: &mut Frame, view: &AgentView, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} — inspect ", view.meta.name));

    let Some(info) = &view.info else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "no metadata yet",
                Style::default().fg(Color::DarkGray),
            ))
            .block(block),
            area,
        );
        return;
    };

    // Width available for text inside the border.
    let inner = area.width.saturating_sub(2) as usize;
    let key = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);

    let field = |label: &str, val: &str| {
        Line::from(vec![
            Span::styled(format!("{label}: "), key),
            Span::raw(val.to_string()),
        ])
    };

    let mut body: Vec<Line> = vec![
        field("model", &info.model),
        field(
            "permission mode",
            if info.permission_mode.is_empty() {
                "-"
            } else {
                &info.permission_mode
            },
        ),
    ];

    // One section per capability list: a bold/cyan `Header (N):` line, then the
    // comma-joined names wrapped across as many lines as needed.
    let sections: [(&str, &[String]); 5] = [
        ("Tools", &info.tools),
        ("Skills", &info.skills),
        ("Plugins", &info.plugins),
        ("Slash-commands", &info.slash_commands),
        ("Subagents", &info.agents),
    ];
    for (label, items) in sections {
        body.push(Line::from(String::new()));
        body.push(Line::from(Span::styled(
            format!("{label} ({}):", items.len()),
            key,
        )));
        if items.is_empty() {
            body.push(Line::from(Span::styled(
                "  (none)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for wrapped in wrap_joined(items, ", ", inner) {
                body.push(Line::from(wrapped));
            }
        }
    }

    frame.render_widget(Paragraph::new(body).block(block), area);
}

/// Join `items` with `sep` and hard-wrap the result so no line exceeds `width`
/// columns. Individual items longer than `width` occupy their own line (never
/// split mid-item). Returns at least one (possibly empty) line.
fn wrap_joined(items: &[String], sep: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (idx, item) in items.iter().enumerate() {
        let piece = if idx + 1 < items.len() {
            format!("{item}{sep}")
        } else {
            item.clone()
        };
        if !cur.is_empty() && cur.chars().count() + piece.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        cur.push_str(&piece);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// The bottom input bar (the caller supplies the full label + text; a block
/// cursor is appended). Used for both the spawn prompt and agent messages.
fn draw_prompt_bar(frame: &mut Frame, text: &str, area: Rect) {
    let line = format!("{text}\u{2588}");
    let style = Style::default().fg(Color::Black).bg(Color::Cyan);
    frame.render_widget(Paragraph::new(line).style(style), area);
}

/// Find an agent view by id.
fn find(views: &[AgentView], id: AgentId) -> Option<&AgentView> {
    views.iter().find(|v| v.meta.id == id)
}

/// Master zone on the left, a vertical side stack of the rest on the right.
fn draw_master_stack(
    frame: &mut Frame,
    views: &[AgentView],
    master_id: AgentId,
    stack_ids: &[AgentId],
    focus: Option<AgentId>,
    scroll: u16,
    inline: Option<(&GateView, AgentId)>,
    area: Rect,
    chrome: Chrome,
) {
    let master = find(views, master_id);
    let stack: Vec<&AgentView> = stack_ids.iter().filter_map(|i| find(views, *i)).collect();

    // No side stack (or a single agent): the master fills the whole area.
    if stack.is_empty() {
        match master {
            Some(m) => draw_pane(frame, m, focus == Some(m.meta.id), scroll, inline, area, chrome),
            None => draw_empty(frame, area),
        }
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    match master {
        Some(m) => draw_pane(frame, m, focus == Some(m.meta.id), scroll, inline, cols[0], chrome),
        None => draw_empty(frame, cols[0]),
    }
    draw_stack(frame, &stack, focus, scroll, inline, cols[1], chrome);
}

/// Show only the given agents, in order, as equal vertical rows (approval
/// triage / plain stack fallback).
fn draw_triage(
    frame: &mut Frame,
    views: &[AgentView],
    ids: &[AgentId],
    focus: Option<AgentId>,
    scroll: u16,
    inline: Option<(&GateView, AgentId)>,
    area: Rect,
    chrome: Chrome,
) {
    let panes: Vec<&AgentView> = ids.iter().filter_map(|i| find(views, *i)).collect();
    if panes.is_empty() {
        draw_empty(frame, area);
        return;
    }
    draw_stack(frame, &panes, focus, scroll, inline, area, chrome);
}

/// Split `area` into equal vertical rows, one per view.
fn draw_stack(
    frame: &mut Frame,
    panes: &[&AgentView],
    focus: Option<AgentId>,
    scroll: u16,
    inline: Option<(&GateView, AgentId)>,
    area: Rect,
    chrome: Chrome,
) {
    let n = panes.len() as u32;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Ratio(1, n); panes.len()])
        .split(area);
    for (view, rect) in panes.iter().zip(rows.iter()) {
        draw_pane(frame, view, focus == Some(view.meta.id), scroll, inline, *rect, chrome);
    }
}

/// Draw one agent pane: a bordered block titled with its status bar, body is the
/// PTY tail. Urgent agents get a red, bold, `!`-marked frame so they stand out
/// even in a style-blind (symbol-only) snapshot.
fn draw_pane(
    frame: &mut Frame,
    view: &AgentView,
    focused: bool,
    scroll: u16,
    inline: Option<(&GateView, AgentId)>,
    area: Rect,
    chrome: Chrome,
) {
    // Scroll offset applies only to the focused pane; see the bottom-follow math
    // below. The offset is consumed there.
    let urgent = view.is_urgent();
    // Urgent (red) wins over focus (cyan); a `▸` marker makes focus visible in a
    // style-blind snapshot. With no focused pane the output is unchanged.
    let style = if urgent {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else if is_subagent(view) {
        // A spawned sub-agent's pane: dim cyan, subordinate to urgent/focus.
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(Color::Gray)
    };
    // Live status indicator (animated spinner + fine activity + turn timer),
    // colored by activity, sits at the FRONT of the title so it reads at a
    // glance across every pane; the rest keeps the border style.
    let focus_marker = if focused { "\u{25b8} " } else { "" };
    let elapsed_secs = chrome.elapsed.get(&view.meta.id).copied();
    let (indicator, ind_style) = pane_indicator(view, chrome.tick, elapsed_secs);
    let title = Line::from(vec![
        Span::styled(format!("{focus_marker}{indicator}  "), ind_style),
        Span::styled(status_bar(view), style),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title);
    let inner_area = block.inner(area);
    frame.render_widget(&block, area);

    // A shell pane renders its live terminal grid instead of an agent transcript
    // (no gate/scrollback — the emulator owns the viewport). Record the inner
    // size so the binary can resize the PTY to match.
    if let Some(screen) = chrome.shells.get(&view.meta.id) {
        chrome
            .shell_sizes
            .borrow_mut()
            .insert(view.meta.id, (inner_area.height, inner_area.width));
        render_shell_grid(frame, inner_area, screen, focused);
        return;
    }

    // A live gate for THIS pane renders as an inline menu pinned to the bottom
    // (Claude-style): reserve its rows, the transcript takes what's left above.
    let gate = inline.and_then(|(g, t)| (view.meta.id == t).then_some(g));
    let (body_area, gate_area) = match gate {
        Some(g) => {
            let body_h: usize = g
                .body
                .iter()
                .map(|tl| tl.text.lines().count().max(1))
                .sum();
            let (group_lines, _) = gate_group_lines(g);
            // separator + plan body + every group's lines + hint. Cap so at least
            // one transcript row remains; `gate_inline_lines` windows to fit.
            let want = 1 + body_h + group_lines.len() + 1;
            let cap = (inner_area.height as usize).saturating_sub(1).max(1);
            let gate_h = want.min(cap);
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(gate_h as u16)])
                .split(inner_area);
            (rows[0], Some(rows[1]))
        }
        None => (inner_area, None),
    };

    let inner = inner_area.width as usize; // width inside the border
    let visible = body_area.height as usize; // transcript rows
    let body: Vec<Line> = view
        .tail
        .iter()
        .flat_map(|tl| render_transcript_line(tl, inner))
        .collect();
    // Bottom-follow: with no scrollback offset, pin the viewport to the newest
    // lines (the last `visible` rows). One `Line` is treated as one row; the
    // `Paragraph` clips anything wider. When the body fits, `max_off` is 0 and the
    // scroll is a no-op — so panes that fit render identically to before.
    let max_off = body.len().saturating_sub(visible);
    // PgUp raises `scroll` to reveal older lines; PgDn / scroll == 0 returns to the
    // bottom. Only the focused pane honours the offset; others always follow.
    let scroll_off = if focused { scroll as usize } else { 0 };
    let y = max_off
        .saturating_sub(scroll_off)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(Paragraph::new(body).scroll((y, 0)), body_area);

    if let (Some(g), Some(ga)) = (gate, gate_area) {
        let lines = gate_inline_lines(g, inner, ga.height as usize);
        frame.render_widget(Paragraph::new(lines), ga);
    }
}

/// Map an emulator color to a ratatui color. `Default` becomes `Reset` so the
/// terminal's own default shows through.
fn shell_color(c: ShellColor) -> Color {
    match c {
        ShellColor::Default => Color::Reset,
        ShellColor::Idx(i) => Color::Indexed(i),
        ShellColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Render a shell pane's terminal grid into `area`. Cells are coalesced into
/// style-runs per row for efficiency; the cursor cell is reversed when the pane
/// is focused and the cursor is visible. The grid is clipped to `area`.
fn render_shell_grid(frame: &mut Frame, area: Rect, screen: &ShellScreen, focused: bool) {
    let rows = (screen.rows).min(area.height);
    let cols = (screen.cols).min(area.width);
    let show_cursor = focused && !screen.hide_cursor;
    let (cur_row, cur_col) = screen.cursor;

    let mut lines: Vec<Line> = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        let mut run = String::new();
        let mut run_style = Style::default();
        for c in 0..cols {
            let cell = screen.cell(r, c);
            let (text, mut style) = match cell {
                Some(cell) => {
                    let mut style = Style::default()
                        .fg(shell_color(cell.fg))
                        .bg(shell_color(cell.bg));
                    if cell.bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell.italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if cell.underline {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell.inverse {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    let text = if cell.contents.is_empty() {
                        " ".to_string()
                    } else {
                        cell.contents.clone()
                    };
                    (text, style)
                }
                None => (" ".to_string(), Style::default()),
            };
            // The cursor cell is drawn reversed so it stands out (block cursor).
            if show_cursor && r == cur_row && c == cur_col {
                style = style.add_modifier(Modifier::REVERSED);
            }
            // Coalesce consecutive same-styled cells into one span.
            if style == run_style {
                run.push_str(&text);
            } else {
                if !run.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut run), run_style));
                }
                run = text;
                run_style = style;
            }
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, run_style));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The live decision menu rendered INLINE at the bottom of a pane (Claude-style):
/// a separator, the plan body (if any), every question group, and a key hint.
/// When the content is taller than `max_h`, a window of rows is shown scrolled so
/// the cursor row stays visible (the hint is always pinned last).
fn gate_inline_lines(gate: &GateView, width: usize, max_h: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);

    let (group_lines, cursor_in_groups) = gate_group_lines(gate);
    let body: Vec<Line<'static>> = gate
        .body
        .iter()
        .flat_map(|tl| render_transcript_line(tl, width))
        .collect();

    // content = separator + plan body + group lines. Track the cursor's index.
    let mut content: Vec<Line<'static>> = Vec::with_capacity(1 + body.len() + group_lines.len());
    content.push(Line::from(Span::styled("\u{2500}".repeat(width.min(48)), dim)));
    content.extend(body);
    let groups_start = content.len();
    content.extend(group_lines);
    let cursor_abs = groups_start + cursor_in_groups;

    let hint = Line::from(Span::styled(gate_hint(gate, "hide"), dim));

    // Reserve the last row for the hint; window the rest around the cursor.
    let avail = max_h.saturating_sub(1).max(1);
    let mut out: Vec<Line<'static>> = if content.len() <= avail {
        content
    } else {
        let start = cursor_abs
            .saturating_sub(avail / 2)
            .min(content.len() - avail);
        content.into_iter().skip(start).take(avail).collect()
    };
    out.push(hint);
    out
}

/// Whether this pane is a spawned sub-agent. The frozen `AgentView` carries no
/// parent field, so the core marks sub-agents with a `↳ ` name prefix (see
/// awm-core `SUBAGENT_PREFIX`) and the TUI detects it here.
fn is_subagent(view: &AgentView) -> bool {
    view.meta.name.starts_with('\u{21b3}')
}

/// A placeholder pane when there is nothing to show.
fn draw_empty(frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" no agents ");
    frame.render_widget(Paragraph::new("").block(block), area);
}

/// Render one transcript line to styled ratatui line(s), Claude Code-style:
/// green `⏺` tool calls, dim `⎿` results (red on error), and markdown for text.
fn render_transcript_line(tl: &TranscriptLine, width: usize) -> Vec<Line<'static>> {
    let plain = |text: &str, style: Style| vec![Line::from(Span::styled(text.to_string(), style))];
    match tl.kind {
        LineKind::Text => markdown_lines(&tl.text, width),
        LineKind::ToolCall => plain(&tl.text, Style::default().fg(Color::Green)),
        LineKind::ToolResult => plain(&tl.text, Style::default().fg(Color::DarkGray)),
        LineKind::ToolError => plain(&tl.text, Style::default().fg(Color::Red)),
        LineKind::Thinking => plain(
            &tl.text,
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ),
        LineKind::System => plain(&tl.text, Style::default().fg(Color::DarkGray)),
        LineKind::Note => plain(&tl.text, Style::default().fg(Color::Cyan)),
        LineKind::Approval => plain(
            &tl.text,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

/// Markdown → styled lines (block-aware): fenced code, headers, GFM tables,
/// blockquotes, bullet + ordered lists, horizontal rules, and inline styling.
fn markdown_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut in_code = false;
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim_end();
        let ltrim = trimmed.trim_start();

        // Fenced code blocks.
        if ltrim.starts_with("```") {
            in_code = !in_code;
            out.push(Line::from(Span::styled(trimmed.to_string(), dim)));
            i += 1;
            continue;
        }
        if in_code {
            out.push(Line::from(Span::styled(
                trimmed.to_string(),
                Style::default().fg(Color::Cyan),
            )));
            i += 1;
            continue;
        }

        // GFM table: a `| … |` row followed by a `|---|` separator.
        if is_table_row(trimmed) && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let (rendered, consumed) = render_table(&lines[i..], width);
            out.extend(rendered);
            i += consumed;
            continue;
        }

        // Horizontal rule.
        if is_hr(ltrim) {
            out.push(Line::from(Span::styled(
                "\u{2500}".repeat(width.clamp(3, 100)),
                dim,
            )));
            i += 1;
            continue;
        }

        // ATX headers.
        if let Some(h) = ltrim
            .strip_prefix("### ")
            .or_else(|| ltrim.strip_prefix("## "))
            .or_else(|| ltrim.strip_prefix("# "))
        {
            out.push(Line::from(Span::styled(
                h.to_string(),
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            i += 1;
            continue;
        }

        // Blockquote.
        if ltrim == ">" || ltrim.starts_with("> ") {
            let q = ltrim.strip_prefix("> ").unwrap_or("");
            let mut spans = vec![Span::styled("\u{258e} ".to_string(), dim)];
            spans.extend(inline_spans(q));
            out.push(Line::from(spans));
            i += 1;
            continue;
        }

        // Unordered list.
        if let Some(rest) = ltrim
            .strip_prefix("- ")
            .or_else(|| ltrim.strip_prefix("* "))
            .or_else(|| ltrim.strip_prefix("+ "))
        {
            let mut spans = vec![Span::styled("\u{2022} ".to_string(), Style::default().fg(Color::Yellow))];
            spans.extend(inline_spans(rest));
            out.push(Line::from(spans));
            i += 1;
            continue;
        }

        // Ordered list: `N. text`.
        if let Some((num, rest)) = split_ordered(ltrim) {
            let mut spans = vec![Span::styled(
                format!("{num}. "),
                Style::default().fg(Color::Yellow),
            )];
            spans.extend(inline_spans(rest));
            out.push(Line::from(spans));
            i += 1;
            continue;
        }

        out.push(Line::from(inline_spans(trimmed)));
        i += 1;
    }
    if out.is_empty() {
        out.push(Line::from(String::new()));
    }
    out
}

/// Column alignment parsed from a table's separator row.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
    Center,
}

fn is_table_row(s: &str) -> bool {
    s.contains('|')
}

fn is_table_separator(s: &str) -> bool {
    let cells = split_cells(s);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let t = c.trim();
            !t.is_empty() && t.chars().all(|ch| ch == '-' || ch == ':')
        })
}

fn is_hr(s: &str) -> bool {
    let t: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3
        && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

fn split_ordered(s: &str) -> Option<(String, &str)> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &s[digits.len()..];
    let rest = rest.strip_prefix(". ")?;
    Some((digits, rest))
}

/// Split a table row into trimmed cells, dropping the optional leading/trailing pipe.
fn split_cells(s: &str) -> Vec<String> {
    let t = s.trim().trim_start_matches('|').trim_end_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// Truncate (char-safe, with `…`) and pad `s` to exactly `w` columns per `align`.
fn fit(s: &str, w: usize, align: Align) -> String {
    let chars: Vec<char> = s.chars().collect();
    let cell: String = if chars.len() > w {
        if w == 0 {
            String::new()
        } else {
            chars[..w - 1].iter().collect::<String>() + "\u{2026}"
        }
    } else {
        s.to_string()
    };
    let pad = w.saturating_sub(cell.chars().count());
    match align {
        Align::Left => format!("{cell}{}", " ".repeat(pad)),
        Align::Right => format!("{}{cell}", " ".repeat(pad)),
        Align::Center => {
            let l = pad / 2;
            format!("{}{cell}{}", " ".repeat(l), " ".repeat(pad - l))
        }
    }
}

/// Render a GFM table starting at `lines[0]`; returns the styled lines and the
/// number of source lines consumed.
fn render_table(lines: &[&str], width: usize) -> (Vec<Line<'static>>, usize) {
    let dim = Style::default().fg(Color::DarkGray);
    let header = split_cells(lines[0]);
    let ncols = header.len().max(1);

    let aligns: Vec<Align> = split_cells(lines[1])
        .iter()
        .map(|c| {
            let t = c.trim();
            let l = t.starts_with(':');
            let r = t.ends_with(':');
            match (l, r) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            }
        })
        .collect();
    let align_of = |c: usize| aligns.get(c).copied().unwrap_or(Align::Left);

    // Body rows: until a non-table line.
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut consumed = 2;
    for line in &lines[2..] {
        if !is_table_row(line.trim_end()) || is_table_separator(line) {
            break;
        }
        rows.push(split_cells(line));
        consumed += 1;
    }

    let cell = |row: &[String], c: usize| row.get(c).cloned().unwrap_or_default();

    // Natural column widths from content.
    let mut cols: Vec<usize> = (0..ncols)
        .map(|c| {
            let mut w = header.get(c).map(|s| s.chars().count()).unwrap_or(0);
            for r in &rows {
                w = w.max(cell(r, c).chars().count());
            }
            w.max(1)
        })
        .collect();

    // Shrink to fit: total = 1 + Σ(colw + 3). Trim widest columns until it fits.
    let overhead = 1 + 3 * ncols;
    let budget = width.saturating_sub(overhead).max(ncols);
    while cols.iter().sum::<usize>() > budget {
        let (idx, _) = cols.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
        if cols[idx] <= 1 {
            break;
        }
        cols[idx] -= 1;
    }

    let rule = |left: &str, mid: &str, right: &str| {
        let mut s = String::from(left);
        for (c, w) in cols.iter().enumerate() {
            s.push_str(&"\u{2500}".repeat(w + 2));
            s.push_str(if c + 1 == ncols { right } else { mid });
        }
        Line::from(Span::styled(s, dim))
    };
    let data_row = |row: &[String], bold: bool| {
        let mut spans = vec![Span::styled("\u{2502}".to_string(), dim)];
        for c in 0..ncols {
            spans.push(Span::raw(" "));
            let text = fit(&strip_inline(&cell(row, c)), cols[c], align_of(c));
            let style = if bold {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(text, style));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("\u{2502}".to_string(), dim));
        }
        Line::from(spans)
    };

    let mut out = vec![
        rule("\u{250c}", "\u{252c}", "\u{2510}"),
        data_row(&header, true),
        rule("\u{251c}", "\u{253c}", "\u{2524}"),
    ];
    for r in &rows {
        out.push(data_row(r, false));
    }
    out.push(rule("\u{2514}", "\u{2534}", "\u{2518}"));
    (out, consumed)
}

/// Strip inline markdown markers to plain text (for fixed-width table cells).
fn strip_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' | '*' | '_' => i += 1,
            '[' => {
                // [text](url) -> text
                if let Some(close) = (i + 1..chars.len()).find(|&j| chars[j] == ']') {
                    out.extend(&chars[i + 1..close]);
                    i = close + 1;
                    if i < chars.len() && chars[i] == '(' {
                        if let Some(p) = (i..chars.len()).find(|&j| chars[j] == ')') {
                            i = p + 1;
                        }
                    }
                } else {
                    out.push('[');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Inline markdown within a line: `**bold**`, `*italic*`/`_italic_`, `` `code` ``.
fn inline_spans(s: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::raw(std::mem::take(buf)));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        // [text](url) -> underlined text (url dropped)
        if c == '[' {
            if let Some(close) = (i + 1..chars.len()).find(|&j| chars[j] == ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(paren) = (close + 2..chars.len()).find(|&j| chars[j] == ')') {
                        flush(&mut buf, &mut spans);
                        let label: String = chars[i + 1..close].iter().collect();
                        spans.push(Span::styled(
                            label,
                            Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
                        ));
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        // `code`
        if c == '`' {
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '`') {
                flush(&mut buf, &mut spans);
                let code: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(code, Style::default().fg(Color::Cyan)));
                i = end + 1;
                continue;
            }
        }
        // **bold**
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_double(&chars, i + 2, '*') {
                flush(&mut buf, &mut spans);
                let b: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(b, Style::default().add_modifier(Modifier::BOLD)));
                i = end + 2;
                continue;
            }
        }
        // *italic* or _italic_
        if (c == '*' || c == '_') && i + 1 < chars.len() && chars[i + 1] != c {
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == c) {
                flush(&mut buf, &mut spans);
                let it: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(it, Style::default().add_modifier(Modifier::ITALIC)));
                i = end + 1;
                continue;
            }
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// Find the start index of the next `cc` double-marker at/after `from`.
fn find_double(chars: &[char], from: usize, c: char) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == c && chars[j + 1] == c)
}

/// The per-agent status bar: `[! ]<id> <name> [tags] <state> <total>tok[ · <mode>]`.
///
/// The leading `! ` marker is emitted for urgent agents so the highlight is
/// visible in plain-text snapshots (which carry symbols, not styles). When the
/// agent's [`AgentInfo`] is known, its permission mode is appended as ` · <mode>`.
fn status_bar(view: &AgentView) -> String {
    let marker = if view.is_urgent() { "! " } else { "" };
    let mode = view
        .info
        .as_ref()
        .map(|i| i.permission_mode.as_str())
        .filter(|m| !m.is_empty())
        .map(|m| format!(" \u{b7} {m}"))
        .unwrap_or_default();
    // The live state now leads the title via `pane_indicator`; the status bar
    // keeps identity + tags + token count + mode.
    format!(
        "{marker}{id} {name} [{tags}] {total}tok{mode}",
        id = view.meta.id,
        name = view.meta.name,
        tags = format_tags(view.meta.tags),
        total = view.tokens.total(),
    )
}

/// Render tag flags as a compact `#1,#2` list, or `-` when untagged.
fn format_tags(tags: Tags) -> String {
    let parts: Vec<String> = (1..=9u8)
        .filter(|n| tags.contains(Tags::slot(*n)))
        .map(|n| format!("#{n}"))
        .collect();
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(",")
    }
}

/// Braille spinner frames for the "working" animation.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// The spinner glyph for frame `tick`.
fn spinner(tick: u64) -> char {
    SPINNER[(tick as usize) % SPINNER.len()]
}

/// Format a turn timer like ` 8s` / ` 1m05s`; empty when no elapsed is known.
fn timer(elapsed_secs: Option<u64>) -> String {
    match elapsed_secs {
        Some(s) if s >= 60 => format!(" {}m{:02}s", s / 60, s % 60),
        Some(s) => format!(" {s}s"),
        None => String::new(),
    }
}

/// The tool name from a `⏺ Name(args)` header line (or the whole remainder).
fn tool_name(header: &str) -> &str {
    let rest = header.trim_start_matches('⏺').trim_start();
    rest.split('(').next().unwrap_or(rest).trim()
}

/// Derive the fine-grained activity of a *working* agent from its transcript
/// tail: reasoning (`✻`), a tool call (`⏺ Name`), a finished tool (`⎿`), or a
/// streaming reply (plain text). Continuation lines (diff `  +/-`, wrapped tool
/// output) are skipped so an edit's diff doesn't mask the tool name. Returns the
/// label and its color. Display-only heuristic — coarse `Working` is the source
/// of truth for *whether* it's active.
fn activity(view: &AgentView) -> (String, Color) {
    for line in view.tail.iter().rev() {
        let t = line.text.as_str();
        if t.is_empty() {
            continue;
        }
        if t.starts_with('✻') {
            return ("thinking".to_string(), Color::Cyan);
        }
        if t.starts_with('⏺') {
            return (format!("running {}", tool_name(t)), Color::Yellow);
        }
        if t.starts_with('⎿') {
            // A tool just returned — the agent is processing the result.
            return ("working".to_string(), Color::Yellow);
        }
        if t.starts_with("  ") {
            continue; // diff / wrapped-output continuation — keep scanning back
        }
        if line.kind == LineKind::Text {
            return ("responding".to_string(), Color::Green);
        }
        break;
    }
    ("working".to_string(), Color::Yellow)
}

/// The live status indicator shown at the front of a pane title: an animated
/// spinner + activity + turn timer while working, or a static state glyph
/// otherwise. Returns the text and its style.
fn pane_indicator(view: &AgentView, tick: u64, elapsed_secs: Option<u64>) -> (String, Style) {
    let bold = Modifier::BOLD;
    match view.state {
        AgentState::Idle => ("· idle".to_string(), Style::default().fg(Color::DarkGray)),
        AgentState::Done => ("✓ done".to_string(), Style::default().fg(Color::Green)),
        AgentState::Failed => (
            "✗ failed".to_string(),
            Style::default().fg(Color::Red).add_modifier(bold),
        ),
        AgentState::BlockedOnApproval => (
            format!("⏸ waiting: approval{}", timer(elapsed_secs)),
            Style::default().fg(Color::Red).add_modifier(bold),
        ),
        AgentState::Working => {
            let (word, color) = activity(view);
            (
                format!("{} {word}{}", spinner(tick), timer(elapsed_secs)),
                Style::default().fg(color).add_modifier(bold),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awm_proto::{AgentInfo, AgentMeta, TokenUsage};
    use ratatui::backend::TestBackend;

    /// Render the `TestBackend` buffer to plain text for a stable snapshot.
    fn buffer_to_string(backend: &TestBackend) -> String {
        let buf = backend.buffer();
        let area = *buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf.get(x, y).symbol());
            }
            out.push('\n');
        }
        out
    }

    fn names(prefix: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i}")).collect()
    }

    fn card_view() -> AgentView {
        AgentView {
            meta: AgentMeta {
                id: AgentId(3),
                name: "inspector".into(),
                tags: Tags::empty(),
                cwd: "/home/dev/proj".into(),
                started_at: 0,
                urgent: false,
            },
            state: AgentState::Working,
            tokens: TokenUsage::default(),
            info: Some(AgentInfo {
                model: "claude-opus-4-8".into(),
                permission_mode: "acceptEdits".into(),
                tools: vec![
                    "Bash".into(),
                    "Read".into(),
                    "Edit".into(),
                    "Write".into(),
                    "Grep".into(),
                    "Glob".into(),
                ],
                skills: names("skill-", 16),
                plugins: vec!["plug-a".into(), "plug-b".into()],
                slash_commands: vec!["/review".into(), "/test".into(), "/deploy".into()],
                agents: vec!["Explore".into(), "Plan".into()],
                session_id: None,
            }),
            tail: vec![],
        }
    }

    fn view_with_info(info: Option<AgentInfo>) -> AgentView {
        AgentView {
            meta: AgentMeta {
                id: AgentId(0),
                name: "builder".into(),
                tags: Tags::empty(),
                cwd: "/p".into(),
                started_at: 0,
                urgent: false,
            },
            state: AgentState::Working,
            tokens: TokenUsage {
                input: 100,
                output: 20,
            },
            info,
            tail: vec![],
        }
    }

    #[test]
    fn card_renders_populated_info() {
        let mut tui = AwmTui::new(TestBackend::new(60, 30)).unwrap();
        let views = vec![card_view()];
        tui.draw(
            &views,
            &LayoutCmd::Monocle(AgentId(3)),
            Some(AgentId(3)),
            None,
            0,
            true,
            false,
            None,
            None,
            None,
            &[],
            &HashMap::new(),
        )
        .unwrap();
        insta::assert_snapshot!("card_populated", buffer_to_string(tui.backend()));
    }

    #[test]
    fn card_without_info_shows_placeholder() {
        let mut view = card_view();
        view.info = None;
        let mut tui = AwmTui::new(TestBackend::new(40, 8)).unwrap();
        tui.draw(
            &[view],
            &LayoutCmd::Monocle(AgentId(3)),
            Some(AgentId(3)),
            None,
            0,
            true,
            false,
            None,
            None,
            None,
            &[],
            &HashMap::new(),
        )
        .unwrap();
        assert!(buffer_to_string(tui.backend()).contains("no metadata yet"));
    }

    #[test]
    fn wrap_joined_never_exceeds_width() {
        let items = names("tool", 20);
        for line in wrap_joined(&items, ", ", 24) {
            assert!(line.chars().count() <= 24, "line too wide: {line:?}");
        }
    }

    #[test]
    fn status_bar_appends_permission_mode_when_known() {
        let view = view_with_info(Some(AgentInfo {
            permission_mode: "plan".into(),
            ..Default::default()
        }));
        assert_eq!(status_bar(&view), "@0 builder [-] 120tok \u{b7} plan");
    }

    #[test]
    fn status_bar_omits_mode_when_info_absent() {
        // The live state now leads the title via `pane_indicator`, so the bar
        // itself carries only identity + tags + tokens.
        let view = view_with_info(None);
        assert_eq!(status_bar(&view), "@0 builder [-] 120tok");
    }

    #[test]
    fn status_bar_omits_mode_when_empty() {
        let view = view_with_info(Some(AgentInfo::default()));
        assert_eq!(status_bar(&view), "@0 builder [-] 120tok");
    }

    fn view_with_tail(state: AgentState, tail: Vec<TranscriptLine>) -> AgentView {
        AgentView {
            meta: AgentMeta {
                id: AgentId(0),
                name: "a".into(),
                tags: Tags::empty(),
                cwd: "/p".into(),
                started_at: 0,
                urgent: false,
            },
            state,
            tokens: TokenUsage::default(),
            info: None,
            tail,
        }
    }

    #[test]
    fn spinner_cycles() {
        assert_eq!(spinner(0), '⠋');
        assert_eq!(spinner(SPINNER.len() as u64), '⠋');
        assert_ne!(spinner(1), spinner(0));
    }

    #[test]
    fn timer_formats_seconds_and_minutes() {
        assert_eq!(timer(None), "");
        assert_eq!(timer(Some(8)), " 8s");
        assert_eq!(timer(Some(65)), " 1m05s");
    }

    #[test]
    fn tool_name_parses_header() {
        assert_eq!(tool_name("⏺ Bash(ls -la)"), "Bash");
        assert_eq!(tool_name("⏺ Read"), "Read");
    }

    #[test]
    fn activity_distinguishes_thinking_tool_and_reply() {
        use LineKind as K;
        let think = view_with_tail(
            AgentState::Working,
            vec![TranscriptLine::new(K::Thinking, "✻ pondering")],
        );
        assert_eq!(activity(&think).0, "thinking");

        // A tool with its diff after it still reports the tool (scan skips the
        // `  +/-` diff continuation lines).
        let edit = view_with_tail(
            AgentState::Working,
            vec![
                TranscriptLine::new(K::ToolCall, "⏺ Edit(src/main.rs)"),
                TranscriptLine::new(K::ToolError, "  - old"),
                TranscriptLine::new(K::ToolCall, "  + new"),
            ],
        );
        assert_eq!(activity(&edit).0, "running Edit");

        let reply = view_with_tail(
            AgentState::Working,
            vec![TranscriptLine::new(K::Text, "## Summary")],
        );
        assert_eq!(activity(&reply).0, "responding");

        assert_eq!(activity(&view_with_tail(AgentState::Working, vec![])).0, "working");
    }

    #[test]
    fn pane_indicator_animates_working_and_labels_states() {
        let working = view_with_tail(
            AgentState::Working,
            vec![TranscriptLine::new(LineKind::Thinking, "✻ x")],
        );
        assert_eq!(pane_indicator(&working, 0, Some(12)).0, "⠋ thinking 12s");

        assert_eq!(pane_indicator(&view_with_tail(AgentState::Idle, vec![]), 0, None).0, "· idle");
        assert_eq!(
            pane_indicator(&view_with_tail(AgentState::BlockedOnApproval, vec![]), 0, Some(3)).0,
            "⏸ waiting: approval 3s"
        );
        let done = view_with_tail(AgentState::Done, vec![]);
        assert_eq!(pane_indicator(&done, 0, None).0, "✓ done");
    }
}
