## Task: awm-pty — PTY + agent runner (control channel)

**Context.** Implement two things in `crates/awm-pty/src/lib.rs`:
1. **PTY mode** (`CommandSpec`, `PtySession`) — spawn/kill/resize a child in a
   PTY with a bounded output ring buffer. Used for the manual `e` (attach) case.
2. **Agent-runner mode** (`StreamJsonRunner`, `Decision`) — the primary path for
   the killer feature. Spawn the agent via **piped stdio** in stream-json mode,
   expose raw stdout bytes to the parser (Track B), and act as the **permission
   controller**: on a `can_use_tool` gate, write a `control_response` line.

Approval is a control-channel concern, ground-truthed in `docs/approval-findings.md`
(raw shapes in `docs/canusetool-raw.json`). The allow/deny *decision* and layout
wiring are Phase 3 — you only implement the transport/write-path here.

**Scope.** Only `crates/awm-pty/`. Do not touch `awm-proto` (frozen) or other crates.

**Acceptance (make green):**
- `tests/spawn.rs` — `spawn`/`tail`/`resize`/`wait`/`kill` on a PTY.
- `tests/control_channel.rs` — drives `fixtures/mock-agent.py` (NOT live claude):
  `StreamJsonRunner::spawn`, `read()` raw bytes (empty vec = EOF), `answer(request_id,
  Decision::Allow)` must write the control_response the mock expects
  (`{"type":"control_response","response":{"subtype":"success","request_id":<id>,
  "response":{"behavior":"allow","updatedInput":{...}}}}`), so the agent proceeds
  and exits 0. Add unit tests for ring-buffer eviction and a `Decision::Deny` path.

**Don't:** touch `awm-proto`; spawn a live `claude`; add global mutable state.

**Notes.** MSRV 1.75 / edition 2021; `tokio` (process/io) is available. If a dep
bumps MSRV, pin it (`cargo update <crate> --precise <ver>`). `cargo build &&
cargo test` before every commit; conventional commits, one step per commit.
