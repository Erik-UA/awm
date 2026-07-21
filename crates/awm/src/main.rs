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

use awm_core::session::PaneKind;
use awm_core::{plan_layout, Engine, LayoutMode};
use awm_pty::{CommandSpec, Decision, ShellSession};
use awm_proto::{AgentId, AgentMeta, AgentState, LineKind, Renderer, Tags, TranscriptLine};
use awm_tui::keymap::{map_key, Action};
use awm_tui::AwmTui;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
};
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

/// Lines scrolled per mouse-wheel notch (gentler than a PgUp page).
const MOUSE_SCROLL_STEP: u16 = 3;

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

    // Headless stats run: `--probe-approvals` drives a live claude through several
    // approval gates (forcing out-of-cwd writes), records the full `ApprovalCtx`
    // of each, answers allow/deny, and prints approval-gate statistics. Pair with
    // `AWM_CAPTURE_DIR=captures` to also archive the raw stream.
    if args.iter().any(|a| a == "--probe-approvals") {
        return run_probe_approvals();
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

/// One captured approval gate, distilled from its live `ApprovalCtx` for stats.
struct GateStat {
    order: usize,
    at_ms: u128,
    tool: String,
    input_keys: Vec<String>,
    plan_len: Option<usize>,
    has_desc: bool,
    decision_reason: Option<String>,
    has_tool_use_id: bool,
    request_id: String,
    answer: &'static str, // "allow" | "deny"
}

/// Headless statistics run over LIVE approval gates. Spawns one claude and asks
/// it to perform three out-of-cwd file ops (each of which gates), records the
/// full `ApprovalCtx` of every gate, answers with an allow/deny mix (deny the 2nd
/// to exercise the deny wire path), and prints a statistics report. Run with
/// `AWM_CAPTURE_DIR=captures` to also tee the raw stream to `captures/agent-0.jsonl`.
/// Prints, never renders a TUI.
fn run_probe_approvals() -> std::io::Result<()> {
    use std::collections::HashSet;

    // Three distinct out-of-cwd ops → three non-auto-approved gates. In-cwd ops
    // and `echo` auto-approve and never reach the controller, so we target /tmp.
    let prompt = "Do exactly these three steps, one tool call each, and nothing else. \
                  Do not explain. \
                  1) Write the single character a to the file /tmp/awm-probe-a.txt. \
                  2) Write the single character b to the file /tmp/awm-probe-b.txt. \
                  3) Run this shell command: rm -f /tmp/awm-probe-b.txt";

    let mut engine = Engine::new();
    let (spec, prompt, handshake, persistent) = spec_for(&Spawn::Claude(prompt.to_string()));
    engine.spawn(spec, "probe", Tags::empty(), prompt, handshake, persistent)?;

    let start = Instant::now();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stats: Vec<GateStat> = Vec::new();

    println!("── probe-approvals: driving a live claude through approval gates ──");
    while start.elapsed() < Duration::from_secs(120) {
        engine.pump_blocking(Duration::from_millis(200));

        for id in engine.registry().blocked_ordered() {
            let req = match engine.registry().pending_request_id(id) {
                Some(r) => r,
                None => continue,
            };
            if !seen.insert(req.clone()) {
                continue; // already recorded + answered this gate
            }
            // Snapshot the full ApprovalCtx before answering.
            let ctx = engine.registry().record(id).and_then(|r| r.pending.clone());
            // Deny the 2nd gate, allow the rest — exercises both wire responses
            // while letting the run continue to later gates.
            let deny = stats.len() == 1;
            if let Some(c) = &ctx {
                let input_keys: Vec<String> = c
                    .input
                    .as_object()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                let plan_len = c
                    .input
                    .get("plan")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len());
                let stat = GateStat {
                    order: stats.len() + 1,
                    at_ms: start.elapsed().as_millis(),
                    tool: c.tool.clone(),
                    input_keys,
                    plan_len,
                    has_desc: c.description.is_some(),
                    decision_reason: c.decision_reason.clone(),
                    has_tool_use_id: c.tool_use_id.is_some(),
                    request_id: req.clone(),
                    answer: if deny { "deny" } else { "allow" },
                };
                println!(
                    "  gate #{} t={}ms tool={} req={} keys={:?} desc={} reason={:?} tuid={} -> {}",
                    stat.order,
                    stat.at_ms,
                    stat.tool,
                    stat.request_id,
                    stat.input_keys,
                    stat.has_desc,
                    stat.decision_reason,
                    stat.has_tool_use_id,
                    stat.answer,
                );
                stats.push(stat);
            } else {
                println!("  gate req={req} on @{} had no ApprovalCtx (?)", id.0);
            }

            let decision = if deny {
                Decision::Deny("probe: denied to capture the deny path".into())
            } else {
                Decision::Allow
            };
            let _ = engine.answer(id, decision);
        }

        if engine.registry().all_terminal() {
            println!("── all agents terminal ──");
            break;
        }
    }

    print_gate_stats(&stats, start.elapsed());
    engine.shutdown();
    engine.join();
    Ok(())
}

/// Aggregate + print the approval-gate statistics gathered by `run_probe_approvals`.
fn print_gate_stats(stats: &[GateStat], elapsed: Duration) {
    use std::collections::BTreeMap;

    println!("\n════════ APPROVAL-GATE STATISTICS ════════");
    println!("claude:         {}", claude_version());
    println!("run wall-clock: {:?}", elapsed);
    println!("total gates:    {}", stats.len());
    if stats.is_empty() {
        println!("(no gates fired — is claude authenticated? did the ops auto-approve?)");
        println!("═══════════════════════════════════════════");
        return;
    }

    let n = stats.len();
    let mut per_tool: BTreeMap<&str, usize> = BTreeMap::new();
    for s in stats {
        *per_tool.entry(s.tool.as_str()).or_default() += 1;
    }
    println!("\nper-tool:");
    for (t, c) in &per_tool {
        println!("  {t:<16} {c}");
    }

    let desc = stats.iter().filter(|s| s.has_desc).count();
    let reason = stats.iter().filter(|s| s.decision_reason.is_some()).count();
    let tuid = stats.iter().filter(|s| s.has_tool_use_id).count();
    let plan = stats.iter().filter(|s| s.plan_len.is_some()).count();
    println!("\nApprovalCtx field presence (of {n}):");
    println!("  description       {desc}/{n}");
    println!("  decision_reason   {reason}/{n}");
    println!("  tool_use_id       {tuid}/{n}");
    println!("  input.plan        {plan}/{n}");
    println!("  diff              0/{n} (never populated on the wire)");

    let allow = stats.iter().filter(|s| s.answer == "allow").count();
    println!("\nanswered: {allow} allow · {} deny", n - allow);

    let mut reasons: BTreeMap<&str, usize> = BTreeMap::new();
    for s in stats {
        if let Some(r) = &s.decision_reason {
            *reasons.entry(r.as_str()).or_default() += 1;
        }
    }
    if !reasons.is_empty() {
        println!("\ndecision_reason values:");
        for (r, c) in &reasons {
            println!("  [{c}] {r}");
        }
    }
    println!("═══════════════════════════════════════════");
}

/// Best-effort `claude --version` for the stats header (empty on failure).
fn claude_version() -> String {
    std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
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

    // Live shell-console PTYs, keyed by pane id. These bypass the stream-json
    // `Engine` entirely — the registry only tracks a lightweight `Shell` record
    // (for focus/layout/persistence); the process + emulator grid live here.
    let mut shells: std::collections::HashMap<AgentId, ShellSession> =
        std::collections::HashMap::new();
    // One-shot `Ctrl+b` escape: when armed, the next key is an awm hotkey even if
    // a shell pane is focused (tmux-style prefix).
    let mut prefix_pending = false;

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
            // A shell can't resume its process — open a FRESH one in its saved
            // cwd (prior scrollback is dropped; the restored record is history).
            if snap.kind == PaneKind::Shell {
                let spec = CommandSpec::new(shell_program(), snap.meta.cwd.clone());
                if let Ok(sh) = ShellSession::spawn(&spec, SHELL_ROWS, SHELL_COLS) {
                    engine.registry_mut().reactivate(snap.meta.id);
                    shells.insert(snap.meta.id, sh);
                }
                continue;
            }
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
    let mut gate: Option<awm_tui::GateView> = None; // inline decision menu for the focused blocked agent
    let mut gate_target: Option<AgentId> = None; // the agent that menu answers
    let mut gate_req: Option<String> = None; // request_id the menu was built for (persists selection)
    let mut gate_dismissed: Option<String> = None; // a request_id the user hid with Esc (until `e`/next gate)
    let mut scroll: u16 = 0; // focused pane's scrollback offset (0 = follow bottom)
    let mut prev_focus: Option<AgentId> = None; // to snap to bottom on focus change
    let mut show_card = false; // agent inspection card toggle
    let mut show_help = false; // keybinding help overlay (toggled by `?`)
    let mut last_save = Instant::now(); // periodic session autosave
    let mut dirty = false; // a structural change awaiting an immediate save
    use std::collections::{HashMap, HashSet};
    let anim = Instant::now(); // spinner clock (wall-time → smooth animation)
    // Per-agent start of the current active turn, for the status timer. Cleared
    // when an agent leaves the active (Working/Blocked) states.
    let mut work_since: HashMap<AgentId, Instant> = HashMap::new();

    loop {
        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;
            if let Event::Mouse(me) = &ev {
                // Mouse-wheel scrolls the active pane's history via the same
                // `scroll` offset PgUp/PgDn drive (0 = follow newest). Clamped
                // to the focused pane's content by the per-frame bound below.
                match me.kind {
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_add(MOUSE_SCROLL_STEP),
                    MouseEventKind::ScrollDown => {
                        scroll = scroll.saturating_sub(MOUSE_SCROLL_STEP)
                    }
                    _ => {}
                }
            } else if let Event::Key(key) = ev {
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
                } else if engine
                    .registry()
                    .focus()
                    .is_some_and(|f| shells.contains_key(&f))
                    && !prefix_pending
                {
                    // Shell passthrough: the focused pane is a live shell, so
                    // keystrokes go straight to its PTY. `Ctrl+b` arms a one-shot
                    // escape (handled below) so the NEXT key is an awm hotkey.
                    let fid = engine.registry().focus().unwrap();
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl && matches!(key.code, KeyCode::Char('b')) {
                        prefix_pending = true;
                    } else if let Some(sh) = shells.get_mut(&fid) {
                        let _ = sh.write(&encode_key(&key));
                    }
                } else {
                    // Reaching command mode consumes any armed shell escape.
                    prefix_pending = false;
                    // Layout-independent hotkeys: when a Cyrillic layout is
                    // active, map the char back to its QWERTY-position Latin
                    // key (Ctrl+о → Ctrl+j, etc.). The picker filter and text
                    // input branches above keep the real char for typing.
                    let key = awm_tui::keymap::normalize_layout(key);
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    // Direct keys not covered by the shared keymap.
                    match key.code {
                        KeyCode::Char('q') if !ctrl => break,
                        // Inline decision menu (auto-shown for a blocked focused
                        // agent): ↑↓ move, Space toggles a multi-select option,
                        // Enter sends the choice, Esc hides it. Other keys (focus,
                        // scroll, quit) keep working while the menu is up.
                        KeyCode::Up if gate.is_some() => {
                            if let Some(g) = gate.as_mut() {
                                g.move_cursor(-1);
                            }
                        }
                        KeyCode::Down if gate.is_some() => {
                            if let Some(g) = gate.as_mut() {
                                g.move_cursor(1);
                            }
                        }
                        KeyCode::Char(' ') if gate.is_some() => {
                            if let Some(g) = gate.as_mut() {
                                g.toggle();
                            }
                        }
                        KeyCode::Enter if gate.is_some() => {
                            let chosen = gate.as_ref().map(|g| g.chosen()).unwrap_or_default();
                            // The agent is still blocked, so its ctx is still pending.
                            if let Some(t) = gate_target {
                                if let Some(ctx) =
                                    engine.registry().record(t).and_then(|r| r.pending.clone())
                                {
                                    let _ = engine.answer(t, decision_for_gate(&ctx, &chosen));
                                    scroll = 0;
                                }
                            }
                            gate = None;
                            gate_target = None;
                            gate_dismissed = gate_req.take(); // don't reopen mid-resolve
                        }
                        KeyCode::Esc if gate.is_some() => {
                            gate = None;
                            gate_target = None;
                            gate_dismissed = gate_req.take();
                        }
                        // `?` — toggle the keybinding help overlay; Esc closes it.
                        KeyCode::Char('?') if !ctrl => show_help = !show_help,
                        // `Esc` — close the help overlay if open; otherwise
                        // interrupt the focused agent's current turn (the session
                        // stays alive, like Esc in claude). A no-op if the agent
                        // isn't actively working. (Gate/picker/shell/text-input
                        // Esc are handled by their own earlier branches.)
                        KeyCode::Esc => {
                            if show_help {
                                show_help = false;
                            } else if let Some(f) = engine.registry().focus() {
                                let _ = engine.interrupt(f);
                                dirty = true;
                            }
                        }
                        // `i` — talk to an agent. Prefer the pane holding the open
                        // gate (so you can message a blocked agent), else the
                        // focused pane.
                        KeyCode::Char('i') if !ctrl => {
                            if let Some(t) = gate_target.or_else(|| engine.registry().focus()) {
                                input = Some(Input::Message(t, String::new()));
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
                                // A shell pane: kill its PTY and drop the record.
                                // An agent: the usual Engine kill path.
                                if let Some(mut sh) = shells.remove(&f) {
                                    let _ = sh.kill();
                                    engine.registry_mut().remove(f);
                                } else {
                                    engine.kill(f);
                                }
                                dirty = true;
                            }
                        }
                        // `e` — re-open the inline menu after Esc hid it (only when
                        // the focused agent is still blocked); otherwise `e` falls
                        // through to the keymap (EditInline → monocle).
                        KeyCode::Char('e') if !ctrl && focused_is_blocked(&engine) => {
                            gate_dismissed = None; // auto-manage rebuilds it next frame
                        }
                        _ => {
                            if let Some(action) = map_key(key) {
                                match action {
                                    Action::SpawnPrompt => {
                                        input = Some(Input::Spawn(String::new()))
                                    }
                                    // Open an interactive shell console pane in
                                    // the active project's folder and focus it.
                                    Action::SpawnShell => {
                                        spawn_shell(&mut engine, &mut shells);
                                        scroll = 0;
                                        dirty = true;
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

        // Reap shells whose process exited (user typed `exit`) or whose record
        // was removed elsewhere (e.g. a project close), killing orphan PTYs.
        let mut gone: Vec<AgentId> = Vec::new();
        for (id, sh) in shells.iter_mut() {
            if sh.has_exited() || engine.registry().record(*id).is_none() {
                gone.push(*id);
            }
        }
        for id in gone {
            if let Some(mut sh) = shells.remove(&id) {
                let _ = sh.kill();
            }
            engine.registry_mut().remove(id);
            dirty = true;
        }

        let layout = plan_layout(engine.registry(), mode);
        let views = engine.registry().views();
        let focus = engine.registry().focus();

        // Snap to the newest output whenever focus moves to a different pane.
        if focus != prev_focus {
            scroll = 0;
            prev_focus = focus;
        }

        // Auto-surface the inline decision menu for the focused blocked agent
        // (Claude-style). Build it once per pending request so the highlight
        // persists across frames; `Esc` hides it until the next request or `e`.
        let pending_req = focus
            .and_then(|id| engine.registry().record(id))
            .filter(|r| r.state.is_blocked())
            .and_then(|r| r.pending.as_ref().map(|c| c.request_id.clone()));
        match pending_req {
            Some(req) => {
                let hidden = gate_dismissed.as_deref() == Some(req.as_str());
                if !hidden && gate_req.as_deref() != Some(req.as_str()) {
                    let ctx = focus
                        .and_then(|id| engine.registry().record(id))
                        .and_then(|r| r.pending.clone());
                    gate = ctx.as_ref().and_then(gate_from_ctx);
                    gate_target = gate.as_ref().and(focus);
                    gate_req = Some(req);
                }
            }
            None => {
                // Focused agent isn't blocked → no live menu.
                gate = None;
                gate_target = None;
                gate_req = None;
                gate_dismissed = None;
            }
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

        // Live status indicator: advance the spinner from wall-clock (~10 fps)
        // and track how long each agent has been in its current active turn.
        // The timer resets once an agent leaves the Working/Blocked states.
        let now = Instant::now();
        let tick = (anim.elapsed().as_millis() / 100) as u64;
        let mut elapsed: HashMap<AgentId, u64> = HashMap::new();
        let mut active: HashSet<AgentId> = HashSet::new();
        for v in &views {
            if matches!(
                v.state,
                AgentState::Working | AgentState::BlockedOnApproval
            ) {
                let since = *work_since.entry(v.meta.id).or_insert(now);
                elapsed.insert(v.meta.id, now.duration_since(since).as_secs());
                active.insert(v.meta.id);
            }
        }
        work_since.retain(|id, _| active.contains(id));
        tui.set_chrome(tick, elapsed);

        // Snapshot each live shell's terminal grid for rendering.
        let mut screens: std::collections::HashMap<AgentId, awm_pty::ShellScreen> =
            std::collections::HashMap::new();
        for (id, sh) in &shells {
            screens.insert(*id, sh.snapshot());
        }

        tui.draw(
            &views,
            &layout,
            focus,
            bar.as_deref(),
            scroll,
            show_card,
            show_help,
            picker_view.as_ref(),
            gate.as_ref(),
            gate_target,
            &tabs,
            &screens,
        )?;

        // Resize each shell's PTY to the pane it was just drawn in, so
        // full-screen apps (vim, htop) match the visible geometry.
        for (id, (rows, cols)) in tui.shell_sizes() {
            if let Some(sh) = shells.get_mut(&id) {
                let _ = sh.resize(rows.max(1), cols.max(1));
            }
        }

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
    for sh in shells.values_mut() {
        let _ = sh.kill(); // don't leave shell PTYs running after awm exits
    }
    engine.shutdown();
    Ok(())
}

/// The working directory of the active project — where a newly-spawned agent
/// should run. Falls back to the process cwd if the project has no cwd set.
/// Mock `ExitPlanMode`-style plan gate: a markdown plan body over a
/// single-select Proceed/Keep-planning choice. Stand-in until the overlay is fed
/// by a live `ApprovalCtx` (whose `input.plan` carries the real markdown).
/// Whether the focused agent is currently blocked on an approval gate.
fn focused_is_blocked(engine: &Engine) -> bool {
    engine
        .registry()
        .focus()
        .and_then(|id| engine.registry().record(id))
        .map(|r| r.state.is_blocked())
        .unwrap_or(false)
}

/// Build the decision overlay from a LIVE pending gate. Mirrors Claude Code:
/// `ExitPlanMode` → the markdown plan + Proceed/Keep-planning; `AskUserQuestion`
/// → the question + its options (multi-select when the tool asks for it); any
/// other tool → a plain Yes/No gate with the description/reason as context.
/// Returns `None` if the ctx carries nothing to show (e.g. an empty question).
fn gate_from_ctx(ctx: &awm_proto::ApprovalCtx) -> Option<awm_tui::GateView> {
    use awm_tui::{GateGroup, GateOption, GateView};
    let single = |header: &str, prompt: &str, labels: &[&str], multi: bool| GateGroup {
        header: header.to_string(),
        prompt: prompt.to_string(),
        options: labels.iter().map(|l| GateOption::new(*l)).collect(),
        selected: 0,
        multi,
    };
    match ctx.tool.as_str() {
        "ExitPlanMode" => {
            let plan = ctx.input.get("plan").and_then(|v| v.as_str()).unwrap_or("");
            Some(GateView {
                title: "ExitPlanMode".into(),
                body: plan
                    .lines()
                    .map(|l| TranscriptLine::new(LineKind::Text, l))
                    .collect(),
                groups: vec![single("", "", &["Yes, proceed", "No, keep planning"], false)],
                cursor_group: 0,
                cursor_opt: 0,
            })
        }
        "AskUserQuestion" => {
            let questions = ctx.input.get("questions").and_then(|v| v.as_array())?;
            let groups: Vec<GateGroup> = questions
                .iter()
                .filter_map(|q| {
                    let options: Vec<GateOption> = q
                        .get("options")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|o| o.get("label").and_then(|v| v.as_str()))
                                .map(GateOption::new)
                                .collect()
                        })
                        .unwrap_or_default();
                    if options.is_empty() {
                        return None;
                    }
                    Some(GateGroup {
                        header: q.get("header").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        prompt: q.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        options,
                        selected: 0,
                        multi: q.get("multiSelect").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                })
                .collect();
            if groups.is_empty() {
                return None;
            }
            Some(GateView {
                title: "AskUserQuestion".into(),
                body: vec![],
                groups,
                cursor_group: 0,
                cursor_opt: 0,
            })
        }
        other => {
            let mut body = Vec::new();
            if let Some(d) = &ctx.description {
                body.push(TranscriptLine::new(LineKind::Text, d.clone()));
            }
            if let Some(r) = &ctx.decision_reason {
                body.push(TranscriptLine::new(LineKind::Note, r.clone()));
            }
            Some(GateView {
                title: other.to_string(),
                body,
                groups: vec![single("", "", &["Yes", "No"], false)],
                cursor_group: 0,
                cursor_opt: 0,
            })
        }
    }
}

/// Translate the menu's per-group `chosen()` indices into a control-channel
/// decision. ExitPlanMode/plain gates are allow/deny by the first group's first
/// option; `AskUserQuestion` returns every group's picked label(s) via
/// `updatedInput`.
fn decision_for_gate(ctx: &awm_proto::ApprovalCtx, chosen: &[Vec<usize>]) -> Decision {
    match ctx.tool.as_str() {
        "AskUserQuestion" => askq_decision(ctx, chosen),
        // First group, option 0 = "yes/proceed" ⇒ allow; anything else ⇒ deny.
        _ => {
            if chosen.first().and_then(|g| g.first()) == Some(&0) {
                Decision::Allow
            } else {
                Decision::Deny("declined from awm".into())
            }
        }
    }
}

/// Build the `AskUserQuestion` answer: allow with `updatedInput` = the original
/// input plus an `answers` map with an entry for EVERY question (question text →
/// chosen label, or a list of labels for multi-select). Live-confirmed shape —
/// see `docs/gate-answer-findings.md`.
fn askq_decision(ctx: &awm_proto::ApprovalCtx, chosen: &[Vec<usize>]) -> Decision {
    let Some(questions) = ctx.input.get("questions").and_then(|v| v.as_array()) else {
        return Decision::Deny("no question".into());
    };
    let mut answers = serde_json::Map::new();
    for (qi, q) in questions.iter().enumerate() {
        let question = q.get("question").and_then(|v| v.as_str()).unwrap_or("");
        let multi = q.get("multiSelect").and_then(|v| v.as_bool()).unwrap_or(false);
        let empty = Vec::new();
        let opts = q.get("options").and_then(|v| v.as_array()).unwrap_or(&empty);
        let picked = chosen.get(qi).cloned().unwrap_or_default();
        let labels: Vec<String> = picked
            .iter()
            .filter_map(|&i| opts.get(i))
            .filter_map(|o| o.get("label").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .collect();
        if labels.is_empty() {
            continue; // an unanswered multi-select group contributes nothing
        }
        let value = if multi {
            serde_json::Value::Array(labels.into_iter().map(serde_json::Value::String).collect())
        } else {
            serde_json::Value::String(labels[0].clone())
        };
        answers.insert(question.to_string(), value);
    }
    if answers.is_empty() {
        return Decision::Deny("no option selected".into());
    }
    let mut updated = ctx.input.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("answers".into(), serde_json::Value::Object(answers));
    }
    match serde_json::to_string(&updated) {
        Ok(s) => Decision::AllowWith(s),
        Err(_) => Decision::Allow,
    }
}

/// Initial PTY size for a freshly-spawned shell. Corrected to the real pane
/// size on the first frame via `AwmTui::shell_sizes` (see the resize step).
const SHELL_ROWS: u16 = 24;
const SHELL_COLS: u16 = 80;

/// The program a shell pane runs: the user's `$SHELL`, falling back to `bash`.
fn shell_program() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "bash".into())
}

/// Open a fresh interactive shell pane in the active project's folder, focus it,
/// and register its PTY in `shells`. Drops the pane if the shell fails to spawn.
fn spawn_shell(engine: &mut Engine, shells: &mut std::collections::HashMap<AgentId, ShellSession>) {
    let cwd = active_project_cwd(engine);
    let id = engine.registry_mut().alloc_id();
    engine
        .registry_mut()
        .add_shell(AgentMeta::new(id, "shell", cwd.clone(), 0));
    engine.registry_mut().set_focus(id);
    let spec = CommandSpec::new(shell_program(), cwd);
    match ShellSession::spawn(&spec, SHELL_ROWS, SHELL_COLS) {
        Ok(sh) => {
            shells.insert(id, sh);
        }
        Err(_) => {
            engine.registry_mut().remove(id); // don't leave a dead pane behind
        }
    }
}

/// Encode a key press into the bytes a PTY shell expects on stdin. Printable
/// chars pass through as UTF-8; control chars, Enter/Backspace/Tab and the arrow/
/// navigation keys map to their terminal escape sequences.
fn encode_key(key: &crossterm::event::KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl+<letter> → the corresponding C0 control byte (Ctrl+C = 0x03).
                vec![(c as u8) & 0x1f]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        _ => Vec::new(),
    }
}

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
        crossterm::execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen);
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

/// Live-gate mapping: `ApprovalCtx` → `GateView` (display) and `chosen()` →
/// `Decision` (answer). Driven by the real captured envelopes in `fixtures/`
/// (see `docs/gate-answer-findings.md`); never invokes a live claude.
#[cfg(test)]
mod gate_tests {
    use super::{decision_for_gate, gate_from_ctx, Decision};

    /// Build an `ApprovalCtx` from a captured `can_use_tool` fixture envelope.
    fn ctx_from_fixture(name: &str) -> awm_proto::ApprovalCtx {
        let path = format!("{}/../../fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let env: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let req = &env["request"];
        awm_proto::ApprovalCtx {
            tool: req["tool_name"].as_str().unwrap().to_string(),
            input: req["input"].clone(),
            request_id: env["request_id"].as_str().unwrap_or("").to_string(),
            tool_use_id: req["tool_use_id"].as_str().map(String::from),
            description: req["description"].as_str().map(String::from),
            decision_reason: req["decision_reason"].as_str().map(String::from),
            diff: None,
        }
    }

    #[test]
    fn exitplanmode_shows_plan_and_answers_allow_deny() {
        let ctx = ctx_from_fixture("exitplanmode.json");
        let g = gate_from_ctx(&ctx).expect("plan gate");
        assert_eq!(g.title, "ExitPlanMode");
        assert_eq!(g.groups.len(), 1);
        assert_eq!(g.groups[0].options.len(), 2);
        // The markdown plan body is surfaced (the capture's plan mentions "Plan").
        assert!(g.body.iter().any(|l| l.text.contains("Plan")), "plan body missing");
        // Proceed → allow; keep-planning → deny.
        assert!(matches!(decision_for_gate(&ctx, &[vec![0]]), Decision::Allow));
        assert!(matches!(decision_for_gate(&ctx, &[vec![1]]), Decision::Deny(_)));
    }

    #[test]
    fn askuserquestion_maps_options_and_returns_selection() {
        let ctx = ctx_from_fixture("askuserquestion.json");
        let g = gate_from_ctx(&ctx).expect("question gate");
        assert_eq!(g.groups.len(), 1, "single-question fixture → one group");
        assert!(!g.groups[0].options.is_empty(), "no options rendered");
        let first = g.groups[0].options[0].label.clone();
        // Choosing option 0 → allow carrying the chosen label in `answers`.
        match decision_for_gate(&ctx, &[vec![0]]) {
            Decision::AllowWith(json) => {
                assert!(json.contains("\"answers\""), "no answers map: {json}");
                assert!(json.contains(first.as_str()), "chosen label missing: {json}");
            }
            other => panic!("expected AllowWith, got {other:?}"),
        }
        // No selection → deny (the cancel path).
        assert!(matches!(decision_for_gate(&ctx, &[vec![]]), Decision::Deny(_)));
    }

    #[test]
    fn askuserquestion_two_groups_answers_both() {
        let ctx = ctx_from_fixture("askuserquestion-2groups.json");
        let g = gate_from_ctx(&ctx).expect("two-group gate");
        assert_eq!(g.groups.len(), 2, "both question groups rendered");
        let q0 = g.groups[0].prompt.clone();
        let q1 = g.groups[1].prompt.clone();
        let a0 = g.groups[0].options[0].label.clone(); // pick 0 in group 1
        let a1 = g.groups[1].options[1].label.clone(); // pick 1 in group 2
        match decision_for_gate(&ctx, &[vec![0], vec![1]]) {
            Decision::AllowWith(json) => {
                // Both questions AND both picked labels appear in `answers`.
                for needle in [q0.as_str(), q1.as_str(), a0.as_str(), a1.as_str()] {
                    assert!(json.contains(needle), "missing {needle:?} in {json}");
                }
            }
            other => panic!("expected AllowWith, got {other:?}"),
        }
    }

    #[test]
    fn unknown_tool_falls_back_to_yes_no() {
        let ctx = awm_proto::ApprovalCtx {
            tool: "Bash".into(),
            input: serde_json::json!({"command": "rm -rf build"}),
            request_id: "r1".into(),
            tool_use_id: None,
            description: Some("rm -rf build".into()),
            decision_reason: None,
            diff: None,
        };
        let g = gate_from_ctx(&ctx).expect("generic gate");
        assert_eq!(g.title, "Bash");
        assert_eq!(g.groups[0].options.len(), 2); // Yes / No
        assert!(matches!(decision_for_gate(&ctx, &[vec![0]]), Decision::Allow));
        assert!(matches!(decision_for_gate(&ctx, &[vec![1]]), Decision::Deny(_)));
    }
}
