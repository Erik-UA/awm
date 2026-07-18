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

#[test]
fn triage_shows_only_listed_agents_in_order() {
    let mut tui = AwmTui::new(TestBackend::new(80, 24)).unwrap();
    let views = sample_views();

    // Triage lists the urgent agent first, then the builder.
    tui.render(&views, &LayoutCmd::Triage(vec![AgentId(1), AgentId(0)]))
        .unwrap();

    insta::assert_snapshot!("triage", buffer_to_string(tui.backend()));
}
