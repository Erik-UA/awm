# Interrupting a turn via the `interrupt` control_request (live-confirmed)

**Date:** 2026-07-21 · **Harness:** `scripts/capture-interrupt.py`
**Raw:** `docs/interrupt-raw.json`

## Why

awm needs an `Esc` that *stops the current turn but keeps the session alive* — the
equivalent of pressing `Esc` in the claude TUI — as opposed to `Ctrl+x`, which
SIGTERMs the whole persistent process (`Engine::kill`). This note pins the exact
control_request/response shape so `Answerer::interrupt` can be trusted.

## The wire (CONFIRMED live)

Sent mid-stream (while an assistant turn is actively streaming), on the same stdin
control channel as `initialize` / `set_permission_mode`:

```json
{"type":"control_request","request_id":"awm-int","request":{"subtype":"interrupt"}}
```

claude answers with a matching control_response, then promptly truncates the turn:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"awm-int",
  "response":{"still_queued":[]}}}
```

Then a terminal `result` for the aborted turn (elided fields):

```json
{"type":"result","subtype":"error_during_execution","is_error":true,
  "terminal_reason":"aborted_streaming","stop_reason":null,
  "errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"]}
```

Key facts:
- The `awm-int` `request_id` is echoed back — a constant id is fine (fire-and-forget;
  awm does not await it).
- The truncated turn ends as `result` **`subtype:"error_during_execution"`**,
  `is_error:true`, `terminal_reason:"aborted_streaming"` — NOT a clean success. In
  awm this maps to `TurnEnded{ok:false}` (persistent agents stay alive; only a
  non-persistent agent's `result` terminalizes it).
- **The session stays alive.** Immediately after the abort, a follow-up `user`
  turn ("Reply with exactly: STILL-ALIVE") was answered normally — so
  `i`-messaging an interrupted agent works. (Probe leg `session_alive_after: true`.)
- `response.still_queued` was `[]` here; it appears to list any not-yet-started
  queued tool calls at interrupt time. awm ignores it.

## Consequences for awm

- `Answerer::interrupt` (crates/awm-pty) writes exactly the envelope above. ✔
- `Engine::interrupt` (crates/awm-core) only fires for a live, `Working` agent
  (idle/blocked/terminal are no-ops — a blocked agent is answered `y`/`n`, not
  interrupted) and pushes a `⎋ interrupted` note. No `awm-proto` change — the
  aborted turn rides the existing `result → TurnEnded{ok:false}` path.
- Bin binds bare `Esc` in command mode (overlays win first; then interrupt the
  focused agent). Regression: `e2e_mock::interrupt_stops_the_turn_and_keeps_the_session_alive`
  and `::interrupt_is_a_noop_for_a_blocked_agent` (mock stands in for claude — tests
  never run live claude).
