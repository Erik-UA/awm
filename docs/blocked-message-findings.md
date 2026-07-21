# Messaging an agent that's blocked on approval (design + probe)

**Date:** 2026-07-21 · **Harness:** `scripts/capture-blocked-message.py`

## Why

awm lets you press `i` and type to an agent even while it is `BlockedOnApproval`.
But a blocked agent's process is parked mid-`can_use_tool` — it is waiting for a
**`control_response`** (allow/deny), not a `user` turn. So "just send the message"
raises a wire question: is a `{"type":"user",…}` line honored while a
`can_use_tool` gate is still open?

## Probe result — mid-gate injection is NOT reliably honored

`scripts/capture-blocked-message.py` drives live claude to a Write approval gate,
then injects a user message (`"…reply with the word PIVOTED"`) **without** answering
the gate. In the run on 2026-07-21 no `PIVOTED` reply arrived while the gate was
open — the injected turn was not picked up before the control channel was serviced,
and the probe hit its deadline. Read as: **a user turn injected while a gate is
outstanding cannot be relied on.** The gate wants a `control_response` first.

(This is a negative/inconclusive result, not a crisp spec — but it is enough to
reject the "send immediately while blocked" design.)

## Design awm uses: queue-and-flush (never inject mid-gate)

Rather than race the control channel, awm keeps the two concerns separate:

- `Engine::send_message` to a `BlockedOnApproval` agent does **not** write to
  stdin. It stashes the text in `AgentRecord.pending_message`
  (`crates/awm-core/src/registry.rs`) and echoes the `▷ you:` line immediately so
  the UI feels responsive.
- When the gate resolves — the user answers `y`/`n`, `Engine::answer` synthesizes
  `ApprovalResolved` — awm flushes the queued text via `Answerer::send_prompt` as a
  real `user` turn (delivered on the root process for sub-agent panes). Multiple
  messages queued while blocked accumulate (newline-joined) so nothing is lost.
- `Registry::reactivate` clears `pending_message` (a resumed/restored pane starts
  clean).

Net effect for the user: you can talk to a blocked agent at any time; your words
land the instant you answer the gate — no lost input, no mid-gate race. No
`awm-proto` change (the frozen contract is untouched; `pending_message` is
awm-core-private).

Regression: `e2e_mock::message_typed_while_blocked_is_delivered_after_gate_resolves`
(mock stands in for claude — tests never run live claude).

## If a future claude honors mid-gate turns

If a later build reliably accepts a `user` turn while a gate is open, the queue
step can be dropped in favour of an immediate `send_prompt`; the UX (message lands
after you answer) stays acceptable either way, so this is not a blocker.
