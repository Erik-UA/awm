//! Track C acceptance test (RED until Phase 2). Renders a static set of agent
//! views onto a ratatui `TestBackend` and snapshots the result. Fails at
//! `Renderer::render` (`unimplemented!`) until Track C lands.

use awm_proto::{
    AgentId, AgentMeta, AgentState, AgentView, LayoutCmd, LineKind, Renderer, Tags, TokenUsage,
    TranscriptLine,
};
use awm_pty::ShellScreen;
use awm_tui::AwmTui;
use ratatui::backend::TestBackend;
use std::collections::HashMap;

fn tl(kind: LineKind, text: &str) -> TranscriptLine {
    TranscriptLine::new(kind, text)
}

/// No shell panes — the trailing `draw` argument for the agent-only snapshots.
fn no_shells() -> HashMap<AgentId, ShellScreen> {
    HashMap::new()
}

/// A one-row terminal grid holding `text`, padded to `cols`.
fn shell_screen(text: &str, cols: u16) -> ShellScreen {
    use awm_pty::ShellCell;
    let mut cells: Vec<ShellCell> = Vec::with_capacity(cols as usize);
    let mut chars = text.chars();
    for _ in 0..cols {
        let mut cell = ShellCell::default();
        if let Some(ch) = chars.next() {
            cell.contents = ch.to_string();
        }
        cells.push(cell);
    }
    ShellScreen {
        rows: 1,
        cols,
        cursor: (0, text.chars().count() as u16),
        hide_cursor: false,
        cells,
    }
}

fn sample_views() -> Vec<AgentView> {
    vec![
        AgentView {
            meta: AgentMeta {
                id: AgentId(0),
                name: "builder".into(),
                tags: Tags::TAG1,
                cwd: "/home/dev/proj".into(),
                started_at: 1_000,
                urgent: false,
            },
            state: AgentState::Working,
            tokens: TokenUsage {
                input: 3400,
                output: 200,
            },
            info: None,
            tail: vec![
                tl(LineKind::ToolCall, "⏺ Bash(cargo build)"),
                tl(LineKind::ToolResult, "⎿ Finished dev"),
            ],
        },
        AgentView {
            meta: AgentMeta {
                id: AgentId(1),
                name: "cleaner".into(),
                tags: Tags::TAG2,
                cwd: "/home/dev/proj".into(),
                started_at: 2_000,
                urgent: true,
            },
            // The urgent one — should be promoted to master and highlighted.
            state: AgentState::BlockedOnApproval,
            tokens: TokenUsage {
                input: 950,
                output: 70,
            },
            info: None,
            tail: vec![tl(LineKind::Approval, "⏸ approval: Bash rm -rf build")],
        },
    ]
}

/// Render the buffer to plain text for a stable snapshot.
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

#[test]
fn master_stack_promotes_urgent_agent() {
    let mut tui = AwmTui::new(TestBackend::new(80, 24)).unwrap();
    let views = sample_views();

    // The core promotes the blocked agent (@1) to master.
    tui.render(&views, &LayoutCmd::SetMaster(AgentId(1))).unwrap();

    insta::assert_snapshot!("master_stack", buffer_to_string(tui.backend()));
}

#[test]
fn claude_style_transcript() {
    // A window that exercises tool calls, results, and markdown — the Claude-like
    // rendering (⏺ / ⎿ glyphs, headers, bullets, inline code).
    let views = vec![AgentView {
        meta: AgentMeta {
            id: AgentId(0),
            name: "worker".into(),
            tags: Tags::empty(),
            cwd: "/home/dev/proj".into(),
            started_at: 0,
            urgent: false,
        },
        state: AgentState::Working,
        tokens: TokenUsage { input: 1200, output: 90 },
        info: None,
        tail: vec![
            tl(LineKind::ToolCall, "⏺ Bash(ls -la)"),
            tl(LineKind::ToolResult, "⎿ total 8"),
            tl(LineKind::ToolResult, "  src"),
            tl(LineKind::Text, "## Summary\n- **two** entries, one `src` dir\n- all good"),
        ],
    }];

    let mut tui = AwmTui::new(TestBackend::new(70, 14)).unwrap();
    tui.render(&views, &LayoutCmd::Monocle(AgentId(0))).unwrap();
    insta::assert_snapshot!("claude_style", buffer_to_string(tui.backend()));
}

#[test]
fn markdown_table_renders_bordered() {
    let table = "Here are the phases:\n\n\
| Phase | Status | Notes |\n\
|-------|:------:|-------|\n\
| 0 | done | scaffold |\n\
| 1 | done | **frozen** contracts |\n\
| 4 | wip | hardening |\n\n\
> tables render bordered now\n\n\
1. first\n2. second";
    let views = vec![AgentView {
        meta: AgentMeta {
            id: AgentId(0),
            name: "worker".into(),
            tags: Tags::empty(),
            cwd: "/p".into(),
            started_at: 0,
            urgent: false,
        },
        state: AgentState::Working,
        tokens: TokenUsage::default(),
        info: None,
        tail: vec![tl(LineKind::Text, table)],
    }];
    let mut tui = AwmTui::new(TestBackend::new(60, 18)).unwrap();
    tui.render(&views, &LayoutCmd::Monocle(AgentId(0))).unwrap();
    insta::assert_snapshot!("markdown_table", buffer_to_string(tui.backend()));
}

#[test]
fn monocle_full_screens_one_agent() {
    let mut tui = AwmTui::new(TestBackend::new(80, 24)).unwrap();
    let views = sample_views();

    // Zoom the builder (@0) to a full-screen monocle.
    tui.render(&views, &LayoutCmd::Monocle(AgentId(0))).unwrap();

    insta::assert_snapshot!("monocle", buffer_to_string(tui.backend()));
}

/// A pane whose transcript is taller than the pane must autoscroll: the *newest*
/// lines are shown and the oldest scroll off the top. Focusing and scrolling back
/// (a positive offset) reveals the older lines again.
#[test]
fn tall_transcript_autoscrolls_to_bottom() {
    // 30 numbered lines (L00..L29) in a pane far too short to show them all.
    let tail: Vec<TranscriptLine> = (0..30)
        .map(|i| tl(LineKind::System, &format!("L{i:02}")))
        .collect();
    let views = vec![AgentView {
        meta: AgentMeta {
            id: AgentId(0),
            name: "worker".into(),
            tags: Tags::empty(),
            cwd: "/p".into(),
            started_at: 0,
            urgent: false,
        },
        state: AgentState::Working,
        tokens: TokenUsage::default(),
        info: None,
        tail,
    }];

    // Default (scroll == 0) follows the bottom: the last line is visible, the
    // first is not.
    let mut tui = AwmTui::new(TestBackend::new(40, 12)).unwrap();
    tui.render(&views, &LayoutCmd::Monocle(AgentId(0))).unwrap();
    let bottom = buffer_to_string(tui.backend());
    assert!(bottom.contains("L29"), "newest line must be visible:\n{bottom}");
    assert!(!bottom.contains("L00"), "oldest line must be scrolled off:\n{bottom}");

    // Focused + scrolled back reveals the older lines and hides the newest.
    let mut tui = AwmTui::new(TestBackend::new(40, 12)).unwrap();
    tui.draw(
        &views,
        &LayoutCmd::Monocle(AgentId(0)),
        Some(AgentId(0)),
        None,
        u16::MAX, // ScrollTop — clamps to the very top.
        false,
        false,
        None,
        None,
        None,
        &[],
        &no_shells(),
    )
    .unwrap();
    let top = buffer_to_string(tui.backend());
    assert!(top.contains("L00"), "oldest line must be visible at the top:\n{top}");
    assert!(!top.contains("L29"), "newest line must be off-screen at the top:\n{top}");
}

#[test]
fn shell_pane_renders_grid_instead_of_transcript() {
    // A pane present in the shells map draws its terminal grid, not `view.tail`.
    let views = sample_views();
    let id = views[0].meta.id;
    let mut shells = HashMap::new();
    shells.insert(id, shell_screen("$ echo shellmarker", 60));

    let mut tui = AwmTui::new(TestBackend::new(64, 12)).unwrap();
    tui.draw(&views, &LayoutCmd::Monocle(id), Some(id),
             None, 0, false, false, None, None, None, &[], &shells).unwrap();
    let out = buffer_to_string(tui.backend());
    assert!(out.contains("shellmarker"), "shell grid text must render:\n{out}");
}

#[test]
fn triage_shows_only_listed_agents_in_order() {
    let mut tui = AwmTui::new(TestBackend::new(80, 24)).unwrap();
    let views = sample_views();

    // Triage lists the urgent agent first, then the builder.
    tui.render(&views, &LayoutCmd::Triage(vec![AgentId(1), AgentId(0)]))
        .unwrap();

    insta::assert_snapshot!("triage", buffer_to_string(tui.backend()));
}

#[test]
fn project_tab_bar_marks_active_and_urgent() {
    use awm_tui::Tab;
    let mut tui = AwmTui::new(TestBackend::new(40, 8)).unwrap();
    let views = sample_views();
    let tabs = vec![
        Tab { name: "awm".into(), active: true, urgent: false },
        Tab { name: "web".into(), active: false, urgent: true },
        Tab { name: "docs".into(), active: false, urgent: false },
    ];
    tui.draw(&views, &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)), None, 0, false, false, None, None, None, &tabs, &no_shells())
        .unwrap();
    let out = buffer_to_string(tui.backend());
    let top = out.lines().next().unwrap_or_default();
    // Numbered, named tabs on the top row; the urgent project carries a `!`.
    assert!(top.contains("[1:awm]"), "top row: {top:?}");
    assert!(top.contains("[2:web !]"), "urgent tab needs `!`: {top:?}");
    assert!(top.contains("[3:docs]"), "top row: {top:?}");
}

#[test]
fn no_tabs_keeps_layout_unchanged() {
    // With an empty tab slice the top row is NOT a tab bar (byte-identical to the
    // pre-projects layout — this is what the other snapshots rely on).
    let mut tui = AwmTui::new(TestBackend::new(40, 8)).unwrap();
    let views = sample_views();
    tui.draw(&views, &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)), None, 0, false, false, None, None, None, &[], &no_shells())
        .unwrap();
    let out = buffer_to_string(tui.backend());
    let top = out.lines().next().unwrap_or_default();
    assert!(!top.contains("[1:"), "no tab bar without tabs: {top:?}");
}

#[test]
fn help_overlay_lists_key_sections() {
    let mut tui = AwmTui::new(TestBackend::new(70, 40)).unwrap();
    tui.draw(&sample_views(), &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)),
             None, 0, false, true, None, None, None, &[], &no_shells()).unwrap();
    let out = buffer_to_string(tui.backend());
    for needle in ["keybindings", "Screens", "Ctrl+w", "Ctrl+n", "close active", "Ctrl+g", "quit"] {
        assert!(out.contains(needle), "help overlay missing {needle:?}:\n{out}");
    }
}

#[test]
fn picker_overlay_lists_dirs_and_legend() {
    use awm_tui::PickerView;
    let pv = PickerView {
        path: "/home/devops/prototupe".into(),
        entries: vec!["../".into(), "crates/".into()],
        selected: 1,
        query: "cr".into(),
    };
    let mut tui = AwmTui::new(TestBackend::new(70, 14)).unwrap();
    tui.draw(&sample_views(), &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)),
             None, 0, false, false, Some(&pv), None, None, &[], &no_shells()).unwrap();
    let out = buffer_to_string(tui.backend());
    assert!(out.contains("prototupe"), "title path:\n{out}");
    assert!(out.contains("find: cr"), "active filter shown in title:\n{out}");
    assert!(out.contains("crates/"), "a matching subdir:\n{out}");
    assert!(out.contains("../"), "parent entry:\n{out}");
    assert!(out.contains("Tab select"), "footer legend:\n{out}");
}

#[test]
fn gate_overlay_renders_plan_and_single_select() {
    use awm_tui::{GateGroup, GateOption, GateView};
    let gv = GateView {
        title: "ExitPlanMode".into(),
        body: vec![
            tl(LineKind::Text, "## Plan: port the gate"),
            tl(LineKind::Text, "1. Add a GateView overlay"),
        ],
        groups: vec![GateGroup {
            header: String::new(),
            prompt: String::new(),
            options: vec![
                GateOption::new("Yes, proceed"),
                GateOption::new("No, keep planning"),
            ],
            selected: 0,
            multi: false,
        }],
        cursor_group: 0,
        cursor_opt: 0,
    };
    let mut tui = AwmTui::new(TestBackend::new(60, 12)).unwrap();
    tui.draw(&sample_views(), &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)),
             None, 0, false, false, None, Some(&gv), None, &[], &no_shells()).unwrap();
    let out = buffer_to_string(tui.backend());
    assert!(out.contains("ExitPlanMode"), "gate title:\n{out}");
    assert!(out.contains("Plan: port the gate"), "markdown body:\n{out}");
    assert!(out.contains("❯"), "cursor present:\n{out}");
    assert!(out.contains("(●) Yes, proceed"), "radio pick on the first option:\n{out}");
    assert!(out.contains("No, keep planning"), "second option:\n{out}");
    assert!(out.contains("Enter send"), "single-select legend:\n{out}");
}

#[test]
fn gate_inline_renders_menu_inside_the_pane() {
    use awm_tui::{GateGroup, GateOption, GateView};
    let gv = GateView {
        title: "AskUserQuestion".into(),
        body: vec![],
        groups: vec![GateGroup {
            header: "Indentation".into(),
            prompt: "Do you prefer TABS or SPACES?".into(),
            options: vec![GateOption::new("Tabs"), GateOption::new("Spaces")],
            selected: 1, // radio pick = 2nd option
            multi: false,
        }],
        cursor_group: 0,
        cursor_opt: 1, // cursor on the 2nd option
    };
    let views = sample_views();
    let id = views[0].meta.id;
    let mut tui = AwmTui::new(TestBackend::new(72, 16)).unwrap();
    // gate + gate_target set → the menu renders INLINE in that agent's pane
    // (not as a full-screen overlay).
    tui.draw(&views, &LayoutCmd::Monocle(id), Some(id),
             None, 0, false, false, None, Some(&gv), Some(id), &[], &no_shells()).unwrap();
    let out = buffer_to_string(tui.backend());
    assert!(out.contains("Do you prefer TABS or SPACES?"), "question inline:\n{out}");
    assert!(out.contains("Tabs"), "first option:\n{out}");
    assert!(out.contains("❯"), "cursor present:\n{out}");
    assert!(out.contains("(●) Spaces"), "radio pick on the 2nd option:\n{out}");
    assert!(out.contains("Enter send"), "inline key hint:\n{out}");
    // Inline, not a takeover: the pane border/status bar is still there.
    assert!(out.contains("│"), "pane frame remains around the menu:\n{out}");
}

#[test]
fn gate_inline_renders_two_question_groups() {
    use awm_tui::{GateGroup, GateOption, GateView};
    let gv = GateView {
        title: "AskUserQuestion".into(),
        body: vec![],
        groups: vec![
            GateGroup {
                header: "Group 1".into(),
                prompt: "What to test?".into(),
                options: vec![GateOption::new("Unit"), GateOption::new("Integration")],
                selected: 0,
                multi: false,
            },
            GateGroup {
                header: "Group 2".into(),
                prompt: "Where?".into(),
                options: vec![GateOption::new("Local"), GateOption::new("CI")],
                selected: 0,
                multi: false,
            },
        ],
        cursor_group: 1, // cursor in the 2nd group
        cursor_opt: 1,
    };
    let views = sample_views();
    let id = views[0].meta.id;
    let mut tui = AwmTui::new(TestBackend::new(72, 20)).unwrap();
    tui.draw(&views, &LayoutCmd::Monocle(id), Some(id),
             None, 0, false, false, None, Some(&gv), Some(id), &[], &no_shells()).unwrap();
    let out = buffer_to_string(tui.backend());
    // BOTH groups render (headers + options) — the multi-question fix.
    assert!(out.contains("Group 1") && out.contains("Group 2"), "both headers:\n{out}");
    assert!(out.contains("Unit") && out.contains("Integration"), "group 1 options:\n{out}");
    assert!(out.contains("Local") && out.contains("CI"), "group 2 options:\n{out}");
    assert!(out.contains("❯"), "cursor present:\n{out}");
}

#[test]
fn gate_overlay_multi_select_shows_checkboxes() {
    use awm_tui::{GateGroup, GateOption, GateView};
    let mut opts = vec![
        GateOption::new("cargo build"),
        GateOption::new("cargo test"),
        GateOption::new("snapshot review"),
    ];
    opts[1].checked = true; // a pre-checked option renders `[x]`
    let gv = GateView {
        title: "Which checks to run?".into(),
        body: vec![],
        groups: vec![GateGroup {
            header: String::new(),
            prompt: String::new(),
            options: opts,
            selected: 0,
            multi: true,
        }],
        cursor_group: 0,
        cursor_opt: 0,
    };
    let mut tui = AwmTui::new(TestBackend::new(60, 8)).unwrap();
    tui.draw(&sample_views(), &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)),
             None, 0, false, false, None, Some(&gv), None, &[], &no_shells()).unwrap();
    let out = buffer_to_string(tui.backend());
    assert!(out.contains("[ ] cargo build"), "unchecked box:\n{out}");
    assert!(out.contains("[x] cargo test"), "checked box:\n{out}");
    assert!(out.contains("Space toggle"), "multi-select legend:\n{out}");
}
