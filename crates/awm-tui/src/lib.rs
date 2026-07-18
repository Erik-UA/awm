//! Track C — Ratatui rendering and keymap.
//!
//! [`AwmTui`] wraps a ratatui [`Terminal`] over any backend, so tests can drive
//! it with `TestBackend` for snapshotting. [`AwmTui::render`] is a *pure
//! function* of the agent views plus the active [`LayoutCmd`] — it owns no
//! layout policy of its own (the core decides urgent → master promotion, etc.).

#![forbid(unsafe_code)]

use awm_proto::{AgentId, AgentState, AgentView, LayoutCmd, LineKind, Renderer, Tags, TranscriptLine};
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
            // The inspection card takes over the focused agent's view.
            if show_card {
                if let Some(v) = focus.and_then(|id| find(views, id)) {
                    render_card(frame, v, area);
                    return;
                }
            }
            render_into(frame, views, layout, focus, scroll, area);
        })?;
        Ok(())
    }
}

impl<B: Backend> Renderer for AwmTui<B> {
    fn render(&mut self, views: &[AgentView], layout: &LayoutCmd) -> std::io::Result<()> {
        self.draw(views, layout, None, None, 0, false)
    }
}

/// Dispatch a layout command into `area`, optionally marking the focused agent.
/// `scroll` applies to whichever pane is focused.
fn render_into(
    frame: &mut Frame,
    views: &[AgentView],
    layout: &LayoutCmd,
    focus: Option<AgentId>,
    scroll: u16,
    area: Rect,
) {
    match layout {
        LayoutCmd::Monocle(id) => match find(views, *id) {
            Some(v) => draw_pane(frame, v, focus == Some(v.meta.id), scroll, area),
            None => draw_empty(frame, area),
        },
        LayoutCmd::Triage(ids) => draw_triage(frame, views, ids, focus, scroll, area),
        // Both promote a single agent to the master zone; the rest of the roster
        // falls into the side stack in roster order.
        LayoutCmd::SetMaster(id) | LayoutCmd::Focus(id) => {
            let master = *id;
            let stack: Vec<AgentId> = views
                .iter()
                .map(|v| v.meta.id)
                .filter(|i| *i != master)
                .collect();
            draw_master_stack(frame, views, master, &stack, focus, scroll, area);
        }
        // Treat the head of the stack as master, the tail as the stack.
        LayoutCmd::Stack(ids) => match ids.split_first() {
            Some((first, rest)) => {
                draw_master_stack(frame, views, *first, rest, focus, scroll, area)
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
    area: Rect,
) {
    let master = find(views, master_id);
    let stack: Vec<&AgentView> = stack_ids.iter().filter_map(|i| find(views, *i)).collect();

    // No side stack (or a single agent): the master fills the whole area.
    if stack.is_empty() {
        match master {
            Some(m) => draw_pane(frame, m, focus == Some(m.meta.id), scroll, area),
            None => draw_empty(frame, area),
        }
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    match master {
        Some(m) => draw_pane(frame, m, focus == Some(m.meta.id), scroll, cols[0]),
        None => draw_empty(frame, cols[0]),
    }
    draw_stack(frame, &stack, focus, scroll, cols[1]);
}

/// Show only the given agents, in order, as equal vertical rows (approval
/// triage / plain stack fallback).
fn draw_triage(
    frame: &mut Frame,
    views: &[AgentView],
    ids: &[AgentId],
    focus: Option<AgentId>,
    scroll: u16,
    area: Rect,
) {
    let panes: Vec<&AgentView> = ids.iter().filter_map(|i| find(views, *i)).collect();
    if panes.is_empty() {
        draw_empty(frame, area);
        return;
    }
    draw_stack(frame, &panes, focus, scroll, area);
}

/// Split `area` into equal vertical rows, one per view.
fn draw_stack(
    frame: &mut Frame,
    panes: &[&AgentView],
    focus: Option<AgentId>,
    scroll: u16,
    area: Rect,
) {
    let n = panes.len() as u32;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Ratio(1, n); panes.len()])
        .split(area);
    for (view, rect) in panes.iter().zip(rows.iter()) {
        draw_pane(frame, view, focus == Some(view.meta.id), scroll, *rect);
    }
}

/// Draw one agent pane: a bordered block titled with its status bar, body is the
/// PTY tail. Urgent agents get a red, bold, `!`-marked frame so they stand out
/// even in a style-blind (symbol-only) snapshot.
fn draw_pane(frame: &mut Frame, view: &AgentView, focused: bool, scroll: u16, area: Rect) {
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
    let title = if focused {
        format!("\u{25b8} {}", status_bar(view))
    } else {
        status_bar(view)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(Span::styled(title, style));

    let inner = area.width.saturating_sub(2) as usize; // width inside the border
    let visible = area.height.saturating_sub(2) as usize; // rows inside the border
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
    frame.render_widget(Paragraph::new(body).block(block).scroll((y, 0)), area);
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
    format!(
        "{marker}{id} {name} [{tags}] {state} {total}tok{mode}",
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
        assert_eq!(status_bar(&view), "@0 builder [-] working 120tok \u{b7} plan");
    }

    #[test]
    fn status_bar_omits_mode_when_info_absent() {
        // With `info: None` the bar is byte-identical to the pre-mode format —
        // this is what the existing snapshots rely on.
        let view = view_with_info(None);
        assert_eq!(status_bar(&view), "@0 builder [-] working 120tok");
    }

    #[test]
    fn status_bar_omits_mode_when_empty() {
        let view = view_with_info(Some(AgentInfo::default()));
        assert_eq!(status_bar(&view), "@0 builder [-] working 120tok");
    }
}
