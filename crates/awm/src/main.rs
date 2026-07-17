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

fn mock_script() -> PathBuf {
    // Repo-relative in dev; falls back to the fixtures dir next to the binary's
    // manifest at build time.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mock-agent.py")
}

/// Build the spawn spec for `kind`: the command, an optional initial prompt, and
/// whether it needs the control-protocol `initialize` handshake (real claude does).
fn spec_for(kind: &Spawn) -> (CommandSpec, Option<String>, bool) {
    match kind {
        Spawn::Mock => (
            CommandSpec::new("python3", std::env::temp_dir())
                .arg(mock_script().to_string_lossy().to_string()),
            None,
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
            )
        }
        Spawn::Claude(prompt) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
            // `--permission-prompt-tool stdio` routes approval gates to us over the
            // control channel as `can_use_tool` requests (see docs/approval-findings.md).
            let spec = CommandSpec::new("claude", cwd)
                .arg("-p")
                .arg("--input-format")
                .arg("stream-json")
                .arg("--output-format")
                .arg("stream-json")
                .arg("--verbose")
                .arg("--permission-prompt-tool")
                .arg("stdio");
            (spec, Some(prompt.clone()), true)
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
        let (spec, prompt, handshake) = spec_for(&Spawn::Mock);
        engine.spawn(spec, name, Tags::empty(), prompt, handshake)?;
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
        let (spec, prompt, handshake) = spec_for(kind);
        let name = match kind {
            Spawn::Mock => format!("mock-{i}"),
            Spawn::MockChat => format!("chat-{i}"),
            Spawn::Claude(_) => format!("claude-{i}"),
        };
        engine.spawn(spec, name, Tags::empty(), prompt, handshake)?;
    }
    // Remember what Mod+p should spawn (first roster kind, or Mock).
    let spawn_kind = roster.first().cloned().unwrap_or(Spawn::Mock);

    let _guard = TermGuard::enter()?;
    let mut tui = AwmTui::new(CrosstermBackend::new(stdout()))?;
    let mut mode = LayoutMode::Tiling;
    let mut input: Option<Input> = None;

    loop {
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if input.is_some() {
                    // Text-entry mode (spawn prompt or a message to an agent).
                    match key.code {
                        KeyCode::Enter => match input.take() {
                            Some(Input::Spawn(text)) if !text.is_empty() => {
                                spawn_typed(&mut engine, &spawn_kind, text)
                            }
                            Some(Input::Message(id, text)) if !text.is_empty() => {
                                let _ = engine.send_message(id, &text);
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
                                if matches!(action, Action::SpawnPrompt) {
                                    input = Some(Input::Spawn(String::new()));
                                } else {
                                    handle_action(action, &mut engine, &mut mode, &spawn_kind);
                                }
                            }
                        }
                    }
                }
            }
        }

        engine.pump();
        let layout = plan_layout(engine.registry(), mode);
        let focus = engine.registry().focus();
        let bar = input.as_ref().map(|i| i.bar());
        tui.draw(&engine.registry().views(), &layout, focus, bar.as_deref())?;
    }
    Ok(())
}

/// Spawn an agent from a typed prompt, using the app's spawn kind (Claude gets
/// the prompt; mock ignores it but still spawns).
fn spawn_typed(engine: &mut Engine, kind: &Spawn, text: String) {
    let spawn = match kind {
        Spawn::Claude(_) => Spawn::Claude(text),
        Spawn::MockChat => Spawn::MockChat,
        Spawn::Mock => Spawn::Mock,
    };
    let (spec, prompt, handshake) = spec_for(&spawn);
    let _ = engine.spawn(spec, "spawned", Tags::empty(), prompt, handshake);
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
            let (spec, prompt, handshake) = spec_for(spawn_kind);
            let _ = engine.spawn(spec, "spawned", Tags::empty(), prompt, handshake);
        }
        Action::Approve => answer_target(engine, Decision::Allow),
        Action::Deny => answer_target(engine, Decision::Deny("denied from awm".into())),
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
