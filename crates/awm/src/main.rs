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
use std::time::{Duration, Instant};

use awm_core::{plan_layout, Engine, LayoutMode};
use awm_pty::{CommandSpec, Decision};
use awm_proto::{AgentId, AgentState, LineKind, Renderer, Tags};
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

/// The directory browser (`Ctrl+n`): pick a folder to become a new project.
struct Picker {
    /// The directory currently being browsed.
    cwd: PathBuf,
    /// ALL subdirectories of `cwd`, sorted (case-insensitive) — unfiltered.
    dirs: Vec<PathBuf>,
    /// Prefix filter typed by the user (case-insensitive; empty = show all).
    query: String,
    /// Highlighted row over the *filtered* rows: 0 = `../` (unless at root),
    /// else the (`sel`-1)-th match.
    sel: usize,
}

impl Picker {
    fn open(start: PathBuf) -> Self {
        let dirs = list_subdirs(&start);
        Picker { cwd: start, dirs, query: String::new(), sel: 0 }
    }

    /// Whether row 0 is the `../` entry (present unless we're at the root).
    fn has_parent(&self) -> bool {
        self.cwd.parent().is_some()
    }

    /// Subdirectories whose name starts with the (lowercased) query.
    fn matches(&self) -> Vec<&PathBuf> {
        let q = self.query.to_lowercase();
        self.dirs
            .iter()
            .filter(|d| {
                d.file_name()
                    .map(|s| s.to_string_lossy().to_lowercase().starts_with(&q))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Visible rows: optional `../` + filtered matches.
    fn rows(&self) -> usize {
        self.matches().len() + usize::from(self.has_parent())
    }

    fn move_sel(&mut self, delta: isize) {
        let n = self.rows() as isize;
        if n == 0 {
            return;
        }
        self.sel = (((self.sel as isize + delta) % n + n) % n) as usize;
    }

    /// Whether the `../` row is highlighted.
    fn on_parent_row(&self) -> bool {
        self.has_parent() && self.sel == 0
    }

    /// The subdirectory the highlighted row points at (None on `../`).
    fn selected_dir(&self) -> Option<PathBuf> {
        let base = usize::from(self.has_parent());
        self.matches()
            .get(self.sel.checked_sub(base)?)
            .map(|p| (*p).clone())
    }

    /// Move the highlight to the first match after the query changed.
    fn snap_to_first_match(&mut self) {
        self.sel = if self.matches().is_empty() {
            0
        } else {
            usize::from(self.has_parent()) // first match row (after `../`)
        };
    }

    fn push_query(&mut self, c: char) {
        self.query.push(c);
        self.snap_to_first_match();
    }

    /// Delete one query char; returns false if the query was already empty.
    fn pop_query(&mut self) -> bool {
        if self.query.pop().is_some() {
            self.snap_to_first_match();
            true
        } else {
            false
        }
    }

    fn clear_query(&mut self) {
        self.query.clear();
        self.snap_to_first_match();
    }

    /// Descend into the highlighted subdirectory (no-op on `../`).
    fn descend(&mut self) {
        if let Some(d) = self.selected_dir() {
            self.cwd = d;
            self.query.clear();
            self.dirs = list_subdirs(&self.cwd);
            self.sel = 0;
        }
    }

    /// Go up one level, landing the highlight on the folder we came FROM (so a
    /// back-step keeps your place instead of resetting to the top).
    fn parent(&mut self) {
        if let Some(p) = self.cwd.parent() {
            let from = self.cwd.clone();
            self.cwd = p.to_path_buf();
            self.query.clear();
            self.dirs = list_subdirs(&self.cwd);
            self.sel = match self.dirs.iter().position(|d| *d == from) {
                Some(i) => i + usize::from(self.has_parent()),
                None => 0,
            };
        }
    }

    /// Render DTO for the TUI overlay (entries reflect the active filter).
    fn view(&self) -> awm_tui::PickerView {
        let mut entries: Vec<String> = Vec::new();
        if self.has_parent() {
            entries.push("../".into());
        }
        for d in self.matches() {
            let name = d
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            entries.push(format!("{name}/"));
        }
        awm_tui::PickerView {
            path: self.cwd.display().to_string(),
            entries,
            selected: self.sel,
            query: self.query.clone(),
        }
    }
}

/// List the immediate subdirectories of `dir`, sorted case-insensitively by name.
/// Best-effort: an unreadable directory yields an empty list.
fn list_subdirs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    dirs.sort_by_key(|p| {
        p.file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    dirs
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

/// Path to the persisted session file: `$XDG_STATE_HOME/awm/session.json`, else
/// `$HOME/.local/state/awm/session.json`. `None` when neither var is set.
fn session_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))?;
    Some(base.join("awm").join("session.json"))
}

/// Load the saved session, if any. Best-effort: a missing or malformed file
/// yields `None` (we just start clean).
fn load_session() -> Option<awm_core::SessionState> {
    let bytes = std::fs::read(session_path()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist the current session snapshot. Best-effort — a failed save (no HOME,
/// read-only fs, …) is swallowed so it never disrupts the UI.
fn save_session(engine: &Engine) {
    let Some(path) = session_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(&engine.registry().snapshot()) {
        let _ = std::fs::write(&path, json);
    }
}

/// The live-`claude` command spec (persistent stream-json session). With
/// `resume = Some(session_id)` it adds `--resume <id>` to continue a prior
/// session instead of starting a new one.
///
/// `--permission-prompt-tool stdio` routes approval gates to us over the control
/// channel as `can_use_tool` requests (see docs/approval-findings.md). No `-p`:
/// a persistent streaming session that stays alive across turns (like the Agent
/// SDK). stdin is closed on shutdown to end it.
fn claude_spec(cwd: PathBuf, resume: Option<&str>) -> CommandSpec {
    let mut spec = CommandSpec::new("claude", cwd)
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--permission-prompt-tool")
        .arg("stdio")
        .arg("--include-partial-messages");
    if let Some(id) = resume {
        spec = spec.arg("--resume").arg(id);
    }
    spec
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
                // Persistent: the mock emits a `result` (turn end → TurnEnded) after
                // launching its sub-agents, so this exercises that sub-agent panes
                // survive a per-turn end (they retire only on process exit).
                true,
            )
        }
        Spawn::Claude(prompt) => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
            let spec = claude_spec(cwd, None);
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

    // Headless diagnostic: `--probe-subagent <prompt>` spawns a live claude, drives
    // it through a sub-agent approval, and prints which pane each gate lands on.
    if let Some(pos) = args.iter().position(|a| a == "--probe-subagent") {
        let prompt = args.get(pos + 1).cloned().unwrap_or_default();
        return run_probe_subagent(prompt);
    }

    // Headless spike: does `claude --resume <session_id>` continue a persistent
    // stream-json session? Drives one live claude, captures its session_id, kills
    // it, then resumes and asks it to recall context. Gates Phase-4 live restore.
    if args.iter().any(|a| a == "--probe-resume") {
        return run_probe_resume();
    }

    // Build the initial roster: --claude <prompt> (repeatable), else mock agents.
    // `--fresh` ignores any saved session and starts clean.
    let mut roster: Vec<Spawn> = Vec::new();
    let mut fresh = false;
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
            "--fresh" => fresh = true,
            _ => {}
        }
    }
    if roster.is_empty() {
        roster = vec![Spawn::Mock, Spawn::Mock, Spawn::Mock];
    }
    run_interactive(roster, fresh)
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

/// Headless probe for sub-agent approval routing on a LIVE claude. Spawns one
/// claude with `prompt`, then pumps for up to ~120s, printing every pane and which
/// one holds a pending gate each time the blocked set changes, auto-approving each
/// gate so the flow continues. Run with `AWM_CAPTURE_DIR=captures` to also dump the
/// raw stream to `captures/agent-<id>.jsonl`. Prints, never renders a TUI.
fn run_probe_subagent(prompt: String) -> std::io::Result<()> {
    use std::collections::HashSet;

    let mut engine = Engine::new();
    let (spec, prompt, handshake, persistent) = spec_for(&Spawn::Claude(prompt));
    engine.spawn(spec, "root", Tags::empty(), prompt, handshake, persistent)?;

    let start = Instant::now();
    let mut answered: HashSet<String> = HashSet::new();
    let mut last_snapshot = String::new();

    println!("── probe: driving a live claude through sub-agent approvals ──");
    while start.elapsed() < Duration::from_secs(120) {
        engine.pump_blocking(Duration::from_millis(200));

        // Snapshot every pane + its pending gate; print only when it changes.
        let mut snapshot = String::new();
        for v in engine.registry().views() {
            let pending = engine
                .registry()
                .record(v.meta.id)
                .and_then(|r| r.pending.as_ref())
                .map(|c| format!(" GATE tool={} req={}", c.tool, c.request_id))
                .unwrap_or_default();
            snapshot.push_str(&format!(
                "  @{} {:?} state={:?}{}\n",
                v.meta.id.0, v.meta.name, v.state, pending
            ));
        }
        if snapshot != last_snapshot {
            println!("── t={:?} ──\n{}", start.elapsed(), snapshot);
            last_snapshot = snapshot;
        }

        // Auto-approve any new gate (by request_id, once) so the flow proceeds.
        let blocked: Vec<AgentId> = engine.registry().blocked_ordered();
        for id in blocked {
            if let Some(req) = engine.registry().pending_request_id(id) {
                if answered.insert(req.clone()) {
                    let name = engine
                        .registry()
                        .record(id)
                        .map(|r| r.meta.name.clone())
                        .unwrap_or_default();
                    println!("  -> approving gate req={req} on pane @{} {name:?}", id.0);
                    let _ = engine.answer(id, Decision::Allow);
                }
            }
        }

        if engine.registry().all_terminal() {
            println!("── all agents terminal ──");
            break;
        }
    }
    println!("── probe done (t={:?}, {} gate(s) approved) ──", start.elapsed(), answered.len());
    engine.shutdown();
    engine.join();
    Ok(())
}

/// Join the Text lines of an agent's transcript into one string (for the probe).
fn tail_text(engine: &Engine, id: AgentId) -> String {
    engine
        .registry()
        .record(id)
        .map(|r| {
            r.tail
                .iter()
                .filter(|l| matches!(l.kind, LineKind::Text))
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// A short one-line preview of a reply.
fn snippet(s: &str) -> String {
    s.trim().replace('\n', " ").chars().take(120).collect()
}

/// Headless spike (Phase 3 gate): does `claude --resume <session_id>` continue a
/// persistent stream-json session? Turn 1 plants a secret and captures the
/// session_id; turn 2 resumes that id and checks whether the model recalls the
/// secret (i.e. the conversation context was truly restored). Prints a verdict;
/// record it in docs/resume-findings.md. Never renders a TUI.
fn run_probe_resume() -> std::io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    let secret = "BANANA47";

    // --- Turn 1: fresh session, plant a secret, capture the session_id. ---
    println!("── probe-resume: turn 1 (plant secret, capture session_id) ──");
    let mut engine = Engine::new();
    let spec = claude_spec(cwd.clone(), None);
    let id = engine.spawn(
        spec,
        "probe",
        Tags::empty(),
        Some(format!("Remember this secret code: {secret}. Reply with just the word OK.")),
        true,
        true,
    )?;

    let start = std::time::Instant::now();
    let mut session_id: Option<String> = None;
    while start.elapsed() < Duration::from_secs(120) {
        engine.pump_blocking(Duration::from_millis(200));
        for b in engine.registry().blocked_ordered() {
            let _ = engine.answer(b, Decision::Allow);
        }
        if session_id.is_none() {
            session_id = engine
                .registry()
                .record(id)
                .and_then(|r| r.info.as_ref())
                .and_then(|i| i.session_id.clone());
            if let Some(s) = &session_id {
                println!("  captured session_id = {s}");
            }
        }
        // A persistent agent goes Idle once its turn ends.
        let idle = matches!(
            engine.registry().record(id).map(|r| r.state),
            Some(AgentState::Idle)
        );
        if session_id.is_some() && idle && !tail_text(&engine, id).is_empty() {
            break;
        }
    }
    println!("  turn1 reply: {:?}", snippet(&tail_text(&engine, id)));
    engine.shutdown();
    engine.join();

    let Some(sid) = session_id else {
        println!("\n❌ VERDICT: never captured a session_id — cannot test resume.");
        return Ok(());
    };

    // --- Turn 2: resume that session, ask it to recall the secret. ---
    println!("\n── probe-resume: turn 2 (--resume {sid}, recall the secret) ──");
    let mut engine2 = Engine::new();
    let spec2 = claude_spec(cwd, Some(&sid));
    let id2 = engine2.spawn(
        spec2,
        "probe-resumed",
        Tags::empty(),
        Some("What is the secret code I told you earlier? Reply with just the code.".into()),
        true,
        true,
    )?;

    let start2 = std::time::Instant::now();
    let mut ran = false; // did the resumed process produce ANY output?
    let mut recalled = false;
    while start2.elapsed() < Duration::from_secs(120) {
        engine2.pump_blocking(Duration::from_millis(200));
        for b in engine2.registry().blocked_ordered() {
            let _ = engine2.answer(b, Decision::Allow);
        }
        let reply = tail_text(&engine2, id2);
        ran |= !reply.is_empty();
        if reply.contains(secret) {
            recalled = true;
            break;
        }
        match engine2.registry().record(id2).map(|r| r.state) {
            Some(AgentState::Failed) => break, // died — resume likely unsupported here
            Some(AgentState::Idle) if !reply.is_empty() => break,
            _ => {}
        }
    }
    let failed = matches!(
        engine2.registry().record(id2).map(|r| r.state),
        Some(AgentState::Failed)
    );
    println!("  turn2 reply: {:?}", snippet(&tail_text(&engine2, id2)));
    engine2.shutdown();
    engine2.join();

    println!("\n──────── PROBE-RESUME VERDICT ────────");
    println!("  session_id captured : yes ({sid})");
    println!("  resumed process ran : {}", if ran && !failed { "yes" } else { "NO" });
    println!("  recalled the secret : {}", if recalled { "YES (context resumed)" } else { "no" });
    if recalled {
        println!("\n✅ `claude --resume` WORKS in stream-json input mode → Phase 4 live restore is viable.");
    } else if ran && !failed {
        println!("\n⚠️  Resume ran but did not recall context — inspect the reply above.");
    } else {
        println!("\n❌ Resume did not run in stream-json input mode → Phase 4 needs the fallback.");
    }
    Ok(())
}

/// Interactive TUI loop. Restores the terminal on drop (even on panic).
fn run_interactive(roster: Vec<Spawn>, fresh: bool) -> std::io::Result<()> {
    let mut engine = Engine::new();

    // Restore a saved session (projects + panes as read-only history) unless the
    // user asked for a clean start or there is nothing saved. Otherwise, name the
    // default project after cwd and spawn the initial roster.
    let restored = if fresh { None } else { load_session() };
    if let Some(state) = &restored {
        engine.registry_mut().restore(state);
        // Bring each root pane back LIVE via `claude --resume <session_id>` (the
        // spike in docs/resume-findings.md confirms this restores context). Mocks
        // have no session_id → they stay as read-only history; sub-agent panes
        // share the root process and are recreated when the resumed root works.
        for snap in &state.agents {
            if snap.is_subagent || !snap.resumable {
                continue; // mocks (not resumable) stay as read-only history
            }
            if let Some(sid) = &snap.session_id {
                let spec = claude_spec(snap.meta.cwd.clone(), Some(sid));
                let _ = engine.resume_agent(snap.meta.id, spec, None, true, true);
            }
        }
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let name = cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "main".into());
        let active = engine.registry().active();
        engine.registry_mut().set_project_meta(active, name, cwd);

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
    }
    // Remember what Mod+p should spawn (first roster kind, or Mock).
    let spawn_kind = roster.first().cloned().unwrap_or(Spawn::Mock);

    let _guard = TermGuard::enter()?;
    let mut tui = AwmTui::new(CrosstermBackend::new(stdout()))?;
    let mut mode = LayoutMode::Tiling;
    let mut input: Option<Input> = None;
    let mut picker: Option<Picker> = None; // directory browser (Ctrl+n)
    let mut scroll: u16 = 0; // focused pane's scrollback offset (0 = follow bottom)
    let mut prev_focus: Option<AgentId> = None; // to snap to bottom on focus change
    let mut show_card = false; // agent inspection card toggle
    let mut show_help = false; // keybinding help overlay (toggled by `?`)
    let mut last_save = Instant::now(); // periodic session autosave
    let mut dirty = false; // a structural change awaiting an immediate save

    loop {
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if let Some(p) = picker.as_mut() {
                    // Directory-browser mode (Ctrl+n). Typing filters the list by
                    // prefix; ↑↓ move, → open, ← up, Enter selects the highlighted
                    // folder as a new project, Esc clears the filter (else cancels).
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Up => p.move_sel(-1),
                        KeyCode::Down => p.move_sel(1),
                        // Enter / → navigate INTO the highlighted folder (or up on
                        // `../`). Selecting a folder as a project is a separate key.
                        KeyCode::Enter | KeyCode::Right => {
                            if p.on_parent_row() {
                                p.parent();
                            } else {
                                p.descend();
                            }
                        }
                        KeyCode::Left => p.parent(),
                        // Tab SELECTS the folder as a new project: the highlighted
                        // subfolder, or the current directory when `../` is
                        // highlighted. (Letters type into the filter, so select
                        // can't be a letter key like the old `s`.)
                        KeyCode::Tab => {
                            let dir = p.selected_dir().unwrap_or_else(|| p.cwd.clone());
                            let name = dir
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "project".into());
                            let pid = engine.registry_mut().add_project(name, dir);
                            engine.registry_mut().set_active(pid);
                            picker = None;
                            scroll = 0;
                            dirty = true;
                        }
                        KeyCode::Backspace => {
                            if !p.pop_query() {
                                p.parent(); // query already empty → go up
                            }
                        }
                        KeyCode::Char(c) if !ctrl && !c.is_control() => p.push_query(c),
                        KeyCode::Esc => {
                            if p.query.is_empty() {
                                picker = None;
                            } else {
                                p.clear_query();
                            }
                        }
                        _ => {}
                    }
                } else if input.is_some() {
                    // Text-entry mode (spawn prompt or a message to an agent).
                    match key.code {
                        KeyCode::Enter => match input.take() {
                            Some(Input::Spawn(text)) if !text.is_empty() => {
                                spawn_typed(&mut engine, &spawn_kind, text);
                                scroll = 0; // snap to the newest output
                                dirty = true;
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
                        // `?` — toggle the keybinding help overlay; Esc closes it.
                        KeyCode::Char('?') if !ctrl => show_help = !show_help,
                        KeyCode::Esc => show_help = false,
                        // `i` — talk to the focused agent (send a follow-up).
                        KeyCode::Char('i') if !ctrl => {
                            if let Some(f) = engine.registry().focus() {
                                input = Some(Input::Message(f, String::new()));
                            }
                        }
                        // `r` — resume a restored (dead) pane into a live claude
                        // session via its saved session_id. No-op if already live,
                        // or if the pane has no session_id (e.g. a mock).
                        KeyCode::Char('r') if !ctrl => {
                            if let Some(f) = engine.registry().focus() {
                                if !engine.is_live(f) {
                                    let info = engine.registry().record(f).map(|r| {
                                        (
                                            r.resumable,
                                            r.info
                                                .as_ref()
                                                .and_then(|i| i.session_id.clone()),
                                            r.meta.cwd.clone(),
                                        )
                                    });
                                    if let Some((true, Some(sid), cwd)) = info {
                                        let spec = claude_spec(cwd, Some(&sid));
                                        let _ = engine
                                            .resume_agent(f, spec, None, true, true);
                                        scroll = 0;
                                        dirty = true;
                                    }
                                }
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
                                dirty = true;
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
                                    // Open the directory browser to pick a folder
                                    // for a new project — starting at the active
                                    // project's cwd (else the process cwd).
                                    Action::NewProject => {
                                        let start = engine
                                            .registry()
                                            .projects()
                                            .get(engine.registry().active_index())
                                            .map(|p| p.cwd.clone())
                                            .filter(|c| c.is_dir())
                                            .unwrap_or_else(|| {
                                                std::env::current_dir()
                                                    .unwrap_or_else(|_| std::env::temp_dir())
                                            });
                                        picker = Some(Picker::open(start));
                                    }
                                    // Switch screens (projects). Snap scroll so the
                                    // new screen's focused pane follows its newest output.
                                    Action::SwitchProject(n) => {
                                        engine.registry_mut().switch_to(n as usize);
                                        scroll = 0;
                                        dirty = true;
                                    }
                                    Action::NextProject => {
                                        engine.registry_mut().next_project();
                                        scroll = 0;
                                        dirty = true;
                                    }
                                    // Close the active screen (kills its agents).
                                    // The last remaining screen can't be closed.
                                    Action::CloseProject => {
                                        engine.close_active_project();
                                        scroll = 0;
                                        dirty = true;
                                    }
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

        // Build the project tab bar (name + active + cross-screen urgent `!`).
        let tabs: Vec<awm_tui::Tab> = {
            let reg = engine.registry();
            let active = reg.active();
            reg.projects()
                .iter()
                .map(|p| awm_tui::Tab {
                    name: p.name.clone(),
                    active: p.id == active,
                    urgent: reg.project_is_urgent(p.id),
                })
                .collect()
        };

        let bar = input.as_ref().map(|i| i.bar());
        let picker_view = picker.as_ref().map(|p| p.view());
        tui.draw(
            &views,
            &layout,
            focus,
            bar.as_deref(),
            scroll,
            show_card,
            show_help,
            picker_view.as_ref(),
            &tabs,
        )?;

        // Persist immediately after a structural change, else on a ~5s heartbeat
        // (so growing transcripts survive a crash between structural edits).
        if dirty || last_save.elapsed() >= Duration::from_secs(5) {
            save_session(&engine);
            last_save = Instant::now();
            dirty = false;
        }
    }
    // Final save captures the last transcripts before we tear down processes.
    save_session(&engine);
    engine.shutdown();
    Ok(())
}

/// The working directory of the active project — where a newly-spawned agent
/// should run. Falls back to the process cwd if the project has no cwd set.
fn active_project_cwd(engine: &Engine) -> PathBuf {
    engine
        .registry()
        .projects()
        .get(engine.registry().active_index())
        .map(|p| p.cwd.clone())
        .filter(|c| !c.as_os_str().is_empty())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()))
}

/// Spawn an agent from a typed prompt, using the app's spawn kind (Claude gets
/// the prompt; mock ignores it but still spawns). Runs in the ACTIVE project's
/// working directory so an agent on a `web` screen operates in `web`'s folder.
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
    let cwd = active_project_cwd(engine);
    let (mut spec, prompt, handshake, persistent) = spec_for(&spawn);
    spec.cwd = cwd; // run in the active project's folder, not awm's cwd
    let _ = engine.spawn(spec, "spawned", Tags::empty(), prompt, handshake, persistent);
}

fn handle_action(action: Action, engine: &mut Engine, mode: &mut LayoutMode, spawn_kind: &Spawn) {
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
        Action::SpawnPrompt => {
            let cwd = active_project_cwd(engine);
            let (mut spec, prompt, handshake, persistent) = spec_for(spawn_kind);
            spec.cwd = cwd; // run in the active project's folder
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

#[cfg(test)]
mod picker_tests {
    use super::{list_subdirs, Picker};
    use std::fs;
    use std::path::PathBuf;

    /// A fresh unique temp directory containing the given subdirs.
    fn scratch(subs: &[&str]) -> PathBuf {
        let uniq = format!(
            "awm-picker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(uniq);
        for s in subs {
            fs::create_dir_all(root.join(s)).unwrap();
        }
        root
    }

    fn name_of(p: &std::path::Path) -> String {
        p.file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn list_subdirs_is_dirs_only_sorted() {
        let root = scratch(&["bb", "aa", "cc"]);
        fs::write(root.join("file.txt"), b"x").unwrap(); // must be ignored
        let names: Vec<String> = list_subdirs(&root).iter().map(|p| name_of(p)).collect();
        assert_eq!(names, vec!["aa", "bb", "cc"]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn picker_filter_narrows_to_prefix() {
        let root = scratch(&["aa", "ab", "xy"]);
        let mut p = Picker::open(root.clone());
        assert_eq!(p.matches().len(), 3, "no filter shows all");

        p.push_query('a');
        let names: Vec<String> = p.matches().iter().map(|d| name_of(d)).collect();
        assert_eq!(names, vec!["aa", "ab"], "prefix filter narrows the list");
        // Highlight snapped to the first match (row after `../`).
        assert_eq!(p.sel, usize::from(p.has_parent()));
        assert_eq!(name_of(&p.selected_dir().unwrap()), "aa");

        // Case-insensitive.
        p.clear_query();
        p.push_query('X');
        assert_eq!(
            p.matches().iter().map(|d| name_of(d)).collect::<Vec<_>>(),
            vec!["xy"]
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn picker_parent_lands_on_child() {
        let root = scratch(&["proj/inner", "other"]);
        let mut p = Picker::open(root.join("proj"));
        p.move_sel(1); // highlight `inner`
        p.descend();
        assert_eq!(p.cwd, root.join("proj").join("inner"));

        // Going back up lands the highlight on the folder we came from.
        p.parent();
        assert_eq!(p.cwd, root.join("proj"));
        assert_eq!(name_of(&p.selected_dir().unwrap()), "inner");
        fs::remove_dir_all(&root).ok();
    }
}
