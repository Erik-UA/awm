//! Track C acceptance test (RED until Phase 2). Renders a static set of agent
//! views onto a ratatui `TestBackend` and snapshots the result. Fails at
//! `Renderer::render` (`unimplemented!`) until Track C lands.

use awm_proto::{
    AgentId, AgentMeta, AgentState, AgentView, LayoutCmd, LineKind, Renderer, Tags, TokenUsage,
    TranscriptLine,
};
use awm_tui::AwmTui;
use ratatui::backend::TestBackend;

fn tl(kind: LineKind, text: &str) -> TranscriptLine {
    TranscriptLine::new(kind, text)
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
        &[],
    )
    .unwrap();
    let top = buffer_to_string(tui.backend());
    assert!(top.contains("L00"), "oldest line must be visible at the top:\n{top}");
    assert!(!top.contains("L29"), "newest line must be off-screen at the top:\n{top}");
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
    tui.draw(&views, &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)), None, 0, false, false, None, &tabs)
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
    tui.draw(&views, &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)), None, 0, false, false, None, &[])
        .unwrap();
    let out = buffer_to_string(tui.backend());
    let top = out.lines().next().unwrap_or_default();
    assert!(!top.contains("[1:"), "no tab bar without tabs: {top:?}");
}

#[test]
fn help_overlay_lists_key_sections() {
    let mut tui = AwmTui::new(TestBackend::new(70, 30)).unwrap();
    tui.draw(&sample_views(), &LayoutCmd::SetMaster(AgentId(0)), Some(AgentId(0)),
             None, 0, false, true, None, &[]).unwrap();
    let out = buffer_to_string(tui.backend());
    for needle in ["keybindings", "Screens", "Ctrl+w", "Ctrl+n", "close active", "quit"] {
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
             None, 0, false, false, Some(&pv), &[]).unwrap();
    let out = buffer_to_string(tui.backend());
    assert!(out.contains("prototupe"), "title path:\n{out}");
    assert!(out.contains("find: cr"), "active filter shown in title:\n{out}");
    assert!(out.contains("crates/"), "a matching subdir:\n{out}");
    assert!(out.contains("../"), "parent entry:\n{out}");
    assert!(out.contains("Tab select"), "footer legend:\n{out}");
}
