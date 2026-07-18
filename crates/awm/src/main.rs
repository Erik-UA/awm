//! `awm` — the agent window manager runtime.
//!
//! Two modes:
//! - `--demo`: headless, no TTY. Spawns mock agents, prints the frame when they
//!   block (urgent → master), auto-approves, prints the resumed frame. Runnable
//!   anywhere (CI, no terminal).
//! - default: an interactive crossterm TUI. Spawns agents (mock by default, or
//!   `--claude <prompt>` for live agents), rearranges on approval, and answers
//!   gates from the status bar (`y`/`n`).

use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use awm_core::{plan_layout, Engine, LayoutMode};
use awm_pty::{CommandSpec, Decision};
use awm_proto::{AgentId, Renderer, Tags};
use awm_tui::keymap::{map_key, Action};
use awm_tui::AwmTui;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{CrosstermBackend, TestBackend};

/// What kind of agent `Mod+p` / the initial roster spawns.
#[derive(Clone)]
enum Spawn {
    Mock,
    /// A multi-turn mock that holds a conversation (for the dialogue demo).
    MockChat,
    /// A mock that does tool work + markdown (for the Claude-style demo).
    MockWork,
    /// A mock that streams its reply token-by-token (for the streaming demo).
    MockStream,
    /// A persistent per-turn mock (real-claude-like multi-turn; for the convo demo).
    MockConvo,
    /// A mock that replies with a markdown table (for the full-markdown demo).
    MockMd,
    /// A mock that spawns two sub-agents via the `Agent` tool (for the
    /// sub-agent-panes demo).
    MockSubagents,
    Claude(String),
}

/// An in-progress text entry in the bottom bar.
enum Input {
    /// `Ctrl+p` — a prompt for a NEW agent.
    Spawn(String),
    /// `i` — a follow-up message to an existing (focused) agent.
    Message(awm_proto::AgentId, String),
}

impl Input {
    fn buffer(&mut self) -> &mut String {
        match self {
            Input::Spawn(s) | Input::Message(_, s) => s,
        }
    }

    fn bar(&self) -> String {
        match self {
            Input::Spawn(s) => format!("spawn agent> {s}"),
            Input::Message(id, s) => format!("{id} message> {s}"),
        }
    }
}

/// Lines scrolled per PgUp/PgDn.
const SCROLL_STEP: u16 = 8;

/// The next permission mode in the Shift+Tab cycle (bypassPermissions excluded).
fn next_mode(current: &str) -> &'static str {
    match current {
        "default" => "plan",
        "plan" => "acceptEdits",
        _ => "default",
    }
}

fn mock_script() -> PathBuf {
    // Repo-relative in dev; falls back to the fixtures dir next to the binary's
    // manifest at build time.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mock-agent.py")
}

/// Build the spawn spec for `kind`: the command, an optional initial prompt, and
/// whether it needs the control-protocol `initialize` handshake (real claude does).
fn spec_for(kind: &Spawn) -> (CommandSpec, Option<String>, bool, bool) {
    // returns (spec, initial prompt, needs-initialize-handshake, persistent-session)
    match kind {
        Spawn::Mock => (
            CommandSpec::new("python3", std::env::temp_dir())
                .arg(mock_script().to_string_lossy().to_string()),
            None,
            false,
            false,
        ),
        Spawn::MockChat => {
            let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/mock-chat.py");
            (
                CommandSpec::new("python3", std::env::temp_dir())
                    .arg(script.to_string_lossy().to_string()),
                Some("hello".into()),
                false,
                false,
            )
        }
        Spawn::MockWork => {
            let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/mock-work.py");
            (
                CommandSpec::new("python3", std::env::temp_dir())
                    .arg(script.to_string_lossy().to_string()),
                None,
                false,
                false,
            )
        }
        Spawn::MockStream => {
            let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/mock-stream.py");
            (
                CommandSpec::new("python3", std::env::temp_dir())
                    .arg(script.to_string_lossy().to_string()),
                None,
                false,
                false,
            )
        }
        Spawn::MockConvo => {
            let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/mock-convo.py");
            (
                CommandSpec::new("python3", std::env::temp_dir())
                    .arg(script.to_string_lossy().to_string()),
                None,
                false,
                true,
            )
        }
        Spawn::MockMd => {
            let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/mock-md.py");
            (
                CommandSpec::new("python3", std::env::temp_dir())
                    .arg(script.to_string_lossy().to_string()),
                None,
                false,
                false,
            )
        }
        Spawn::MockSubagents => {
            let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/mock-subagents.py");
            (
                CommandSpec::new("python3", std::env::temp_dir())
                    .arg(script.to_string_lossy().to_string()),
                None,
                false,
                false,
            )
        }
        Spawn::Claude(prompt) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
            // `--permission-prompt-tool stdio` routes approval gates to us over the
            // control channel as `can_use_tool` requests (see docs/approval-findings.md).
            // No `-p`: a persistent streaming session that stays alive across
            // turns (like the Agent SDK). stdin is closed on shutdown to end it.
            let spec = CommandSpec::new("claude", cwd)
                .arg("--input-format")
                .arg("stream-json")
                .arg("--output-format")
                .arg("stream-json")
                .arg("--verbose")
                .arg("--permission-prompt-tool")
                .arg("stdio")
                .arg("--include-partial-messages");
            let prompt = if prompt.is_empty() { None } else { Some(prompt.clone()) };
            (spec, prompt, true, true)
        }
    }
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--demo") {
        return run_demo();
    }

    // Build the initial roster: --claude <prompt> (repeatable), else mock agents.
    let mut roster: Vec<Spawn> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--claude" => {
                if let Some(p) = it.next() {
                    roster.push(Spawn::Claude(p.clone()));
                }
            }
            "--mock" => roster.push(Spawn::Mock),
            "--chat" => roster.push(Spawn::MockChat),
            "--work" => roster.push(Spawn::MockWork),
            "--stream" => roster.push(Spawn::MockStream),
            "--convo" => roster.push(Spawn::MockConvo),
            "--md" => roster.push(Spawn::MockMd),
            "--subagents" => roster.push(Spawn::MockSubagents),
            _ => {}
        }
    }
    if roster.is_empty() {
        roster = vec![Spawn::Mock, Spawn::Mock, Spawn::Mock];
    }
    run_interactive(roster)
}

/// Headless demo: block → promote to master → approve → resume.
fn run_demo() -> std::io::Result<()> {
    let mut engine = Engine::new();
    for name in ["builder", "cleaner", "tester"] {
        let (spec, prompt, handshake, persistent) = spec_for(&Spawn::Mock);
        engine.spawn(spec, name, Tags::empty(), prompt, handshake, persistent)?;
    }
    let ids: Vec<AgentId> = engine.registry().order().to_vec();
    let mut tui = AwmTui::new(TestBackend::new(92, 18))?;

    pump_until(&mut engine, |e| {
        ids.iter().all(|id| e.registry().pending_request_id(*id).is_some())
    });
    let layout = plan_layout(engine.registry(), LayoutMode::Tiling);
    tui.render(&engine.registry().views(), &layout)?;
    println!("── all agents blocked on approval → oldest promoted to master (urgent → master) ──");
    print!("{}", frame_text(tui.backend()));

    for id in &ids {
        engine.answer(*id, Decision::Allow)?;
    }
    pump_until(&mut engine, |e| e.registry().all_terminal());
    let layout = plan_layout(engine.registry(), LayoutMode::Tiling);
    tui.render(&engine.registry().views(), &layout)?;
    println!("\n── approved from the bar → agents resumed and finished ──");
    print!("{}", frame_text(tui.backend()));

    engine.join();
    Ok(())
}

/// Interactive TUI loop. Restores the terminal on drop (even on panic).
fn run_interactive(roster: Vec<Spawn>) -> std::io::Result<()> {
    let mut engine = Engine::new();
    for (i, kind) in roster.iter().enumerate() {
        let (spec, prompt, handshake, persistent) = spec_for(kind);
        let name = match kind {
            Spawn::Mock => format!("mock-{i}"),
            Spawn::MockChat => format!("chat-{i}"),
            Spawn::MockWork => format!("work-{i}"),
            Spawn::MockStream => format!("stream-{i}"),
            Spawn::MockConvo => format!("convo-{i}"),
            Spawn::MockMd => format!("md-{i}"),
            Spawn::MockSubagents => format!("subs-{i}"),
            Spawn::Claude(_) => format!("claude-{i}"),
        };
        engine.spawn(spec, name, Tags::empty(), prompt, handshake, persistent)?;
    }
    // Remember what Mod+p should spawn (first roster kind, or Mock).
    let spawn_kind = roster.first().cloned().unwrap_or(Spawn::Mock);

    let _guard = TermGuard::enter()?;
    let mut tui = AwmTui::new(CrosstermBackend::new(stdout()))?;
    let mut mode = LayoutMode::Tiling;
    let mut input: Option<Input> = None;
    let mut scroll: u16 = 0; // focused pane's scrollback offset (0 = follow bottom)
    let mut prev_focus: Option<AgentId> = None; // to snap to bottom on focus change
    let mut show_card = false; // agent inspection card toggle

    loop {
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if input.is_some() {
                    // Text-entry mode (spawn prompt or a message to an agent).
                    match key.code {
                        KeyCode::Enter => match input.take() {
                            Some(Input::Spawn(text)) if !text.is_empty() => {
                                spawn_typed(&mut engine, &spawn_kind, text);
                                scroll = 0; // snap to the newest output
                            }
                            Some(Input::Message(id, text)) if !text.is_empty() => {
                                let _ = engine.send_message(id, &text);
                                scroll = 0; // snap to the reply as it arrives
                            }
                            _ => {}
                        },
                        KeyCode::Esc => input = None,
                        KeyCode::Backspace => {
                            if let Some(i) = input.as_mut() {
                                i.buffer().pop();
                            }
                        }
                        KeyCode::Char(c) => {
                            if let Some(i) = input.as_mut() {
                                i.buffer().push(c);
                            }
                        }
                        _ => {}
                    }
                } else {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    // Direct keys not covered by the shared keymap.
                    match key.code {
                        KeyCode::Char('q') if !ctrl => break,
                        // `i` — talk to the focused agent (send a follow-up).
                        KeyCode::Char('i') if !ctrl => {
                            if let Some(f) = engine.registry().focus() {
                                input = Some(Input::Message(f, String::new()));
                            }
                        }
                        KeyCode::Char('t') if ctrl => {
                            mode = if mode == LayoutMode::Triage {
                                LayoutMode::Tiling
                            } else {
                                LayoutMode::Triage
                            };
                        }
                        KeyCode::Char('x') if ctrl => {
                            if let Some(f) = engine.registry().focus() {
                                engine.kill(f);
                            }
                        }
                        _ => {
                            if let Some(action) = map_key(key) {
                                match action {
                                    Action::SpawnPrompt => {
                                        input = Some(Input::Spawn(String::new()))
                                    }
                                    // Scrollback of the focused pane (Track A
                                    // refines clamping to content height).
                                    Action::ScrollUp => scroll = scroll.saturating_add(SCROLL_STEP),
                                    Action::ScrollDown => {
                                        scroll = scroll.saturating_sub(SCROLL_STEP)
                                    }
                                    Action::ScrollTop => scroll = u16::MAX,
                                    Action::ScrollBottom => scroll = 0,
                                    Action::Inspect => show_card = !show_card,
                                    // Shift+Tab: cycle the focused agent's mode.
                                    Action::CycleMode => {
                                        if let Some(f) = engine.registry().focus() {
                                            let cur = engine
                                                .registry()
                                                .record(f)
                                                .and_then(|r| r.info.as_ref())
                                                .map(|i| i.permission_mode.as_str())
                                                .unwrap_or("default");
                                            let next = next_mode(cur);
                                            let _ = engine.set_permission_mode(f, next);
                                        }
                                    }
                                    other => {
                                        // Any non-scroll action snaps the pane back
                                        // to the newest output.
                                        scroll = 0;
                                        handle_action(other, &mut engine, &mut mode, &spawn_kind)
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        engine.pump();
        let layout = plan_layout(engine.registry(), mode);
        let views = engine.registry().views();
        let focus = engine.registry().focus();

        // Snap to the newest output whenever focus moves to a different pane.
        if focus != prev_focus {
            scroll = 0;
            prev_focus = focus;
        }
        // Clamp the scrollback offset to the focused pane's content so it can't run
        // away (ScrollTop = u16::MAX collapses to the top; nothing underflows). The
        // exact viewport clamp happens in the renderer; this bound just keeps the
        // stored offset proportional to available scrollback. One transcript line
        // maps to at least one row, so summing line counts is a safe upper bound.
        let max_scroll = focus
            .and_then(|id| views.iter().find(|v| v.meta.id == id))
            .map(|v| {
                v.tail
                    .iter()
                    .map(|tl| tl.text.lines().count().max(1))
                    .sum::<usize>()
            })
            .unwrap_or(0)
            .min(u16::MAX as usize) as u16;
        scroll = scroll.min(max_scroll);

        let bar = input.as_ref().map(|i| i.bar());
        tui.draw(&views, &layout, focus, bar.as_deref(), scroll, show_card)?;
    }
    engine.shutdown();
    Ok(())
}

/// Spawn an agent from a typed prompt, using the app's spawn kind (Claude gets
/// the prompt; mock ignores it but still spawns).
fn spawn_typed(engine: &mut Engine, kind: &Spawn, text: String) {
    let spawn = match kind {
        Spawn::Claude(_) => Spawn::Claude(text),
        Spawn::MockChat => Spawn::MockChat,
        Spawn::MockWork => Spawn::MockWork,
        Spawn::MockStream => Spawn::MockStream,
        Spawn::MockConvo => Spawn::MockConvo,
        Spawn::MockMd => Spawn::MockMd,
        Spawn::MockSubagents => Spawn::MockSubagents,
        Spawn::Mock => Spawn::Mock,
    };
    let (spec, prompt, handshake, persistent) = spec_for(&spawn);
    let _ = engine.spawn(spec, "spawned", Tags::empty(), prompt, handshake, persistent);
}

fn handle_action(action: Action, engine: &mut Engine, mode: &mut LayoutMode, spawn_kind: &Spawn) {
    let reg = engine.registry();
    match action {
        Action::FocusNext => engine.registry_mut().focus_step(1),
        Action::FocusPrev => engine.registry_mut().focus_step(-1),
        Action::ZoomMaster => *mode = LayoutMode::Tiling,
        Action::ToggleMonocle => {
            *mode = if *mode == LayoutMode::Monocle {
                LayoutMode::Tiling
            } else {
                LayoutMode::Monocle
            };
        }
        // No interactive session to enter in headless mode — expand the request.
        Action::EditInline => *mode = LayoutMode::Monocle,
        Action::ToggleTag(n) => {
            if let Some(f) = reg.focus() {
                engine.registry_mut().toggle_tag(f, n);
            }
        }
        Action::SpawnPrompt => {
            let (spec, prompt, handshake, persistent) = spec_for(spawn_kind);
            let _ = engine.spawn(spec, "spawned", Tags::empty(), prompt, handshake, persistent);
        }
        Action::Approve => answer_target(engine, Decision::Allow),
        Action::Deny => answer_target(engine, Decision::Deny("denied from awm".into())),
        // Scroll / Inspect are handled in the loop (they touch view-only state).
        _ => {}
    }
}

/// Answer the agent currently occupying the master zone (the oldest blocked).
fn answer_target(engine: &mut Engine, decision: Decision) {
    if let Some(id) = engine.registry().blocked_ordered().first().copied() {
        let _ = engine.answer(id, decision);
    }
}

fn pump_until(engine: &mut Engine, mut done: impl FnMut(&Engine) -> bool) {
    for _ in 0..300 {
        engine.pump_blocking(Duration::from_millis(100));
        if done(engine) {
            return;
        }
    }
}

/// Render a `TestBackend` buffer to plain text (symbols only) for the demo.
fn frame_text(backend: &TestBackend) -> String {
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

/// RAII terminal setup/teardown so the screen is always restored.
struct TermGuard;

impl TermGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        crossterm::execute!(stdout(), EnterAlternateScreen)?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen);
    }
}
