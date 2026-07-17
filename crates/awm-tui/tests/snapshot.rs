//! Track C acceptance test (RED until Phase 2). Renders a static set of agent
//! views onto a ratatui `TestBackend` and snapshots the result. Fails at
//! `Renderer::render` (`unimplemented!`) until Track C lands.

use awm_proto::{
    AgentId, AgentMeta, AgentState, AgentView, LayoutCmd, Renderer, Tags, TokenUsage,
};
use awm_tui::AwmTui;
use ratatui::backend::TestBackend;

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
            tail: vec!["compiling awm-core".into(), "Finished dev".into()],
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
            tail: vec!["awaiting approval: rm -rf build".into()],
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
