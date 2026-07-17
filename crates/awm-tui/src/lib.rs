//! Track C — Ratatui rendering and keymap.
//!
//! [`AwmTui`] wraps a ratatui [`Terminal`] over any backend, so tests can drive
//! it with `TestBackend` for snapshotting. [`AwmTui::render`] is a *pure
//! function* of the agent views plus the active [`LayoutCmd`] — it owns no
//! layout policy of its own (the core decides urgent → master promotion, etc.).

#![forbid(unsafe_code)]

use awm_proto::{AgentId, AgentState, AgentView, LayoutCmd, Renderer, Tags};
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

pub mod keymap;

/// The TUI renderer, generic over a ratatui backend.
pub struct AwmTui<B: Backend> {
    terminal: Terminal<B>,
}

impl<B: Backend> AwmTui<B> {
    /// Build a TUI over `backend` (e.g. `TestBackend` in tests, a crossterm
    /// backend in the real app).
    pub fn new(backend: B) -> std::io::Result<Self> {
        Ok(AwmTui {
            terminal: Terminal::new(backend)?,
        })
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
    pub fn draw(
        &mut self,
        views: &[AgentView],
        layout: &LayoutCmd,
        focus: Option<AgentId>,
        prompt: Option<&str>,
    ) -> std::io::Result<()> {
        self.terminal.draw(|frame| {
            let full = frame.size();
            let area = match prompt {
                Some(text) => {
                    let rows = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(1), Constraint::Length(1)])
                        .split(full);
                    draw_prompt_bar(frame, text, rows[1]);
                    rows[0]
                }
                None => full,
            };
            render_into(frame, views, layout, focus, area);
        })?;
        Ok(())
    }
}

impl<B: Backend> Renderer for AwmTui<B> {
    fn render(&mut self, views: &[AgentView], layout: &LayoutCmd) -> std::io::Result<()> {
        self.draw(views, layout, None, None)
    }
}

/// Dispatch a layout command into `area`, optionally marking the focused agent.
fn render_into(
    frame: &mut Frame,
    views: &[AgentView],
    layout: &LayoutCmd,
    focus: Option<AgentId>,
    area: Rect,
) {
    match layout {
        LayoutCmd::Monocle(id) => match find(views, *id) {
            Some(v) => draw_pane(frame, v, focus == Some(v.meta.id), area),
            None => draw_empty(frame, area),
        },
        LayoutCmd::Triage(ids) => draw_triage(frame, views, ids, focus, area),
        // Both promote a single agent to the master zone; the rest of the roster
        // falls into the side stack in roster order.
        LayoutCmd::SetMaster(id) | LayoutCmd::Focus(id) => {
            let master = *id;
            let stack: Vec<AgentId> = views
                .iter()
                .map(|v| v.meta.id)
                .filter(|i| *i != master)
                .collect();
            draw_master_stack(frame, views, master, &stack, focus, area);
        }
        // Treat the head of the stack as master, the tail as the stack.
        LayoutCmd::Stack(ids) => match ids.split_first() {
            Some((first, rest)) => draw_master_stack(frame, views, *first, rest, focus, area),
            None => draw_empty(frame, area),
        },
    }
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
    area: Rect,
) {
    let master = find(views, master_id);
    let stack: Vec<&AgentView> = stack_ids.iter().filter_map(|i| find(views, *i)).collect();

    // No side stack (or a single agent): the master fills the whole area.
    if stack.is_empty() {
        match master {
            Some(m) => draw_pane(frame, m, focus == Some(m.meta.id), area),
            None => draw_empty(frame, area),
        }
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    match master {
        Some(m) => draw_pane(frame, m, focus == Some(m.meta.id), cols[0]),
        None => draw_empty(frame, cols[0]),
    }
    draw_stack(frame, &stack, focus, cols[1]);
}

/// Show only the given agents, in order, as equal vertical rows (approval
/// triage / plain stack fallback).
fn draw_triage(
    frame: &mut Frame,
    views: &[AgentView],
    ids: &[AgentId],
    focus: Option<AgentId>,
    area: Rect,
) {
    let panes: Vec<&AgentView> = ids.iter().filter_map(|i| find(views, *i)).collect();
    if panes.is_empty() {
        draw_empty(frame, area);
        return;
    }
    draw_stack(frame, &panes, focus, area);
}

/// Split `area` into equal vertical rows, one per view.
fn draw_stack(frame: &mut Frame, panes: &[&AgentView], focus: Option<AgentId>, area: Rect) {
    let n = panes.len() as u32;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Ratio(1, n); panes.len()])
        .split(area);
    for (view, rect) in panes.iter().zip(rows.iter()) {
        draw_pane(frame, view, focus == Some(view.meta.id), *rect);
    }
}

/// Draw one agent pane: a bordered block titled with its status bar, body is the
/// PTY tail. Urgent agents get a red, bold, `!`-marked frame so they stand out
/// even in a style-blind (symbol-only) snapshot.
fn draw_pane(frame: &mut Frame, view: &AgentView, focused: bool, area: Rect) {
    let urgent = view.is_urgent();
    // Urgent (red) wins over focus (cyan); a `▸` marker makes focus visible in a
    // style-blind snapshot. With no focused pane the output is unchanged.
    let style = if urgent {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let title = if focused {
        format!("\u{25b8} {}", status_bar(view))
    } else {
        status_bar(view)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(title, style));

    let body: Vec<Line> = view.tail.iter().map(|l| Line::from(l.as_str())).collect();
    frame.render_widget(Paragraph::new(body).block(block), area);
}

/// A placeholder pane when there is nothing to show.
fn draw_empty(frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" no agents ");
    frame.render_widget(Paragraph::new("").block(block), area);
}

/// The per-agent status bar: `[! ]<id> <name> [tags] <state> <total>tok`.
///
/// The leading `! ` marker is emitted for urgent agents so the highlight is
/// visible in plain-text snapshots (which carry symbols, not styles).
fn status_bar(view: &AgentView) -> String {
    let marker = if view.is_urgent() { "! " } else { "" };
    format!(
        "{marker}{id} {name} [{tags}] {state} {total}tok",
        id = view.meta.id,
        name = view.meta.name,
        tags = format_tags(view.meta.tags),
        state = state_label(view.state),
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

/// A short, stable label for the status bar.
fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::BlockedOnApproval => "BLOCKED",
        AgentState::Done => "done",
        AgentState::Failed => "failed",
    }
}
