# awm — agent window manager

A [dwm](https://dwm.suckless.org/)-style TUI multiplexer for Claude Code agents.
Run many agents at once; the screen **lays itself out from their state**. The
headline feature: when an agent blocks on an approval gate, it is **automatically
promoted to the master zone (urgent → master)** so you can approve it from the
status bar without hunting for it.

```
── all agents blocked on approval → oldest promoted to master (urgent → master) ──
┌! @0 builder [-] BLOCKED 0tok─────────────────────────────┐┌! @1 cleaner [-] BLOCKED 0tok─┐
│● session started                                         ││● session started             │
│→ Write                                                   ││→ Write                       │
│⏸ approval: Write                                         ││⏸ approval: Write             │
│                                                          │└──────────────────────────────┘
│                                                          │┌! @2 tester [-] BLOCKED 0tok──┐
│                                                          ││→ Write                       │
│                                                          ││⏸ approval: Write             │
└──────────────────────────────────────────────────────────┘└──────────────────────────────┘
```

Each window reads like Claude Code itself — `⏺` tool calls with their arguments,
indented `⎿` tool output, and markdown answers (headers, bullets, code):

```
⏺ Bash(ls -la)
⎿ total 8
    src
    Cargo.toml
## Summary
• 2 entries, incl. a `src` directory
```

Replies **stream in token-by-token** (via `--include-partial-messages`), so the
window types out live like a real Claude session.

Press `y` to approve the master agent, `n` to deny — the response goes back over
the agent's control channel and it resumes. No entering the session. Press `i` to
send a follow-up message to the focused agent and watch its reply in the window —
agents run as **persistent** sessions, so you can hold a real multi-turn
conversation (`awm --claude "…"` then keep talking with `i`).

## Quick start

Requires Rust **1.75+** (edition 2021). See [MSRV notes](#building) below.

```bash
# Headless demo — no terminal needed. Agents block → urgent → master → approve → resume.
just demo            # or: cargo run -p awm -- --demo

# Interactive, with live Claude agents (needs a real terminal + the `claude` CLI):
cargo run -p awm -- \
  --claude "create /tmp/awm_probe and write hi into it" \
  --claude "count the lines in Cargo.toml"

# Interactive with built-in mock agents (no API cost):
cargo run -p awm            # spawns three mock agents
```

### Keys (interactive)

| Key | Action |
|-----|--------|
| `y` / `n` | approve / deny the agent in the master zone |
| `i` | message the focused agent — type, `Enter` sends, `Esc` cancels |
| `e` | expand the pending request (monocle) |
| `Ctrl+j` / `Ctrl+k` | move focus down / up |
| `Ctrl+Enter` | back to tiling (focus in master) |
| `Ctrl+m` | toggle monocle (full-screen focus) |
| `Ctrl+t` | toggle triage (only blocked agents, oldest first) |
| `PgUp` / `PgDn` / `Home` / `End` | scroll the focused pane's history (panes auto-follow newest) |
| `Tab` | toggle the agent inspection card (model / mode / skills / plugins / tools) |
| `Ctrl+1..9` | toggle a tag on the focused agent |
| `Ctrl+p` | spawn an agent — type a prompt, `Enter` to launch, `Esc` to cancel |
| `Ctrl+x` | kill the focused agent |
| `q` | quit |

The focused agent is marked with `▸`.

## How it works

```
claude -p --input-format stream-json --output-format stream-json \
       --permission-prompt-tool stdio         (one child process per agent)
  │  stdout: stream-json events        ▲  stdin: control_response (approve/deny)
  ▼                                    │
awm-pty  StreamJsonRunner  ── raw bytes ──▶ awm-parser  StreamParser ──▶ AgentEvent
                                                                          │
                                          awm-core  Registry + layout engine
                                          (urgent → master, triage)       │
                                                                          ▼
                                          awm-tui  Ratatui renderer  (master/stack/monocle/triage)
```

- **`awm-proto`** — frozen contracts: `AgentEvent`, `AgentState` (a total state
  machine), `ApprovalCtx`, `LayoutCmd`, `AgentView`, and the `EventSource` /
  `Renderer` traits.
- **`awm-pty`** — spawns each agent over piped stdio; a `Send` `Answerer` handle
  lets the UI thread answer approvals while a reader thread streams output.
- **`awm-parser`** — turns stream-json into `AgentEvent`s, robust to torn lines
  and garbage.
- **`awm-core`** — the registry, the pure layout engine (urgent → master), and
  the runtime engine (a reader thread per agent, mpsc, approve/deny).
- **`awm-tui`** — a Ratatui renderer; a pure function of the agent views + a
  layout command.
- **`awm`** — the binary that wires it together.

### Approval detection

Tool approval in headless `claude` is **not** a passive stream event — it is an
SDK control-channel `can_use_tool` request, unlocked by
`--permission-prompt-tool stdio` plus an `initialize` handshake. This was
reverse-engineered and verified against Claude Code 2.1.212; see
[`docs/approval-findings.md`](docs/approval-findings.md).

## Building

- Pinned to **rustc 1.75 / edition 2021**; `Cargo.lock` holds MSRV-compatible
  dependency versions (do not add edition-2024 crates).
- `cargo build --workspace` · `cargo test --workspace` (42 tests, incl. a
  headless end-to-end run driven by mock agents — never a live `claude`).
- `fmt`/`clippy` run in CI.
- `scripts/tty-smoke.py` drives the interactive TUI in a real pseudo-terminal
  (via `pyte`) — a way to exercise the interactive path with no terminal attached.

## Status

v0.1 prototype. Phases 0–3 complete (contracts, pty/parser/tui tracks, and the
integrated runtime); Phase 4 (hardening + release) in progress. Out of scope for
v0.1: daemon/detach-attach, a config DSL, worktree management, non-Claude CLIs,
themes.

## License

MIT — see [LICENSE](LICENSE).
