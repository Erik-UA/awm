# Approval-gate + plan-gate statistics (live run)

**Date:** 2026-07-20 · **Claude Code:** 2.1.215 · **cwd:** the awm repo
**Harness:** `awm --probe-approvals` (regular gates) + `scripts/capture-plan-gate.py`
(ExitPlanMode) · **Raw:** `captures/agent-0.jsonl`, `docs/exitplanmode-raw.json`

## Goal

Ground two things the earlier `docs/approval-findings.md` left open on the *live*
wire: (1) real statistics on how ordinary approval gates surface, and (2) the
never-before-captured `ExitPlanMode` plan gate — the payload that will feed the
new `GateView` overlay (`crates/awm-tui/src/lib.rs`).

## Method

- **Regular gates:** `AWM_CAPTURE_DIR=captures ./target/debug/awm --probe-approvals`.
  Spawns one live claude and asks it to do three **out-of-cwd** ops (two `/tmp`
  writes + one `rm`), each of which gates. The probe records the full
  `ApprovalCtx` of every gate from `record(id).pending`, answers an **allow/deny
  mix** (deny the 2nd), and prints the aggregate below.
- **Plan gate:** `python3 scripts/capture-plan-gate.py`. A standalone stream-json
  driver: `initialize` → `set_permission_mode {mode:"plan"}` → a "make a plan"
  prompt → dumps the raw `ExitPlanMode` `can_use_tool` envelope. (Captures the
  *raw* request, which carries fields awm's `ApprovalCtx` drops.)

## Findings — regular approval gates

3 gates fired in one run (deterministic for this prompt):

| # | tool  | input keys            | description | decision_reason                              | tool_use_id | answered |
|---|-------|-----------------------|-------------|----------------------------------------------|-------------|----------|
| 1 | Write | `content`, `file_path`| yes         | `Path is outside allowed working directories`| yes         | allow    |
| 2 | Write | `content`, `file_path`| yes         | `Path is outside allowed working directories`| yes         | deny     |
| 3 | Bash  | `command`, `description`| yes       | *(none)*                                     | yes         | allow    |

`ApprovalCtx` field presence (of 3): `description` 3/3 · `decision_reason` 2/3 ·
`tool_use_id` 3/3 · `input.plan` 0/3 · `diff` **0/3 (never on the wire)**.

Notes:
- Gates arrive fast (t ≈ 6.7–8.0 s) and back-to-back.
- **`decision_reason` is not universal** — the out-of-cwd `Write`s carried
  `"Path is outside allowed working directories"`; the `Bash rm` gated with
  **no** `decision_reason`. Treat `decision_reason` as optional (it already is:
  `Option<String>`).
- Both allow and **deny** were exercised live; deny writes
  `{"behavior":"deny","message":…}` and the flow continued.
- `diff` is confirmed always `None` — nothing populates it in the live path.
- The persistent session does not go terminal, so the probe runs to its 120 s
  cap after the gates resolve (expected; gates all land in the first ~8 s).

## Findings — the `ExitPlanMode` plan gate (raw)

Captured verbatim in `docs/exitplanmode-raw.json`. `set_permission_mode` echoed
`plan`; the plan then surfaced as an ordinary `can_use_tool` gate.

```
envelope keys : type, request_id, request
request keys  : subtype, tool_name, display_name, input,
                tool_use_id, requires_user_interaction
subtype       : can_use_tool
tool_name     : ExitPlanMode   (display_name: ExitPlanMode)
requires_user_interaction : true
input keys    : plan, planFilePath
input.plan    : 1611 chars of markdown (the actual plan body)
```

**The plan gate has a different shape from a tool gate:** it carries
`requires_user_interaction: true` and `input.plan` (+`planFilePath`), but **no**
`description`, **no** `decision_reason`, **no** `permission_suggestions`. The
markdown plan body rides entirely on `input.plan` — exactly what
`demo_plan_gate()` (`crates/awm/src/main.rs`) mocks.

## Consequences for wiring a live `GateView`

`ApprovalCtx.input` is opaque `serde_json::Value`, so everything below is already
retained in `record(id).pending` — the overlay can be built with no proto change:

- **ExitPlanMode → plan overlay:** `tool == "ExitPlanMode"` ⇒ build a
  `GateView::Single` with `body = markdown(ctx.input["plan"])` and options
  `["Yes, proceed", "No, keep planning"]`. Answer Proceed→`Allow`,
  Keep-planning→`Deny`.
- **Ordinary tool gate → options overlay:** empty `body`; title = `ctx.tool`;
  show `ctx.description` / `ctx.decision_reason` when present; options
  `["Yes", "No"]` (+ a "don't ask again" once we surface `permission_suggestions`
  — note it is **not** in `ApprovalCtx` today; the parser drops it, so a
  multi-select AskUserQuestion-style gate would need it added there first).
- Fields awm currently **drops** from the wire (would need parser work if wanted):
  `display_name`, `requires_user_interaction`, `planFilePath`,
  `permission_suggestions`, `decision_reason_type`.

## Reproduce

```
cargo build -p awm
AWM_CAPTURE_DIR=captures ./target/debug/awm --probe-approvals   # regular gates + stats
python3 scripts/capture-plan-gate.py                            # ExitPlanMode envelope
```

Both need an authenticated `claude`; if a run errors on auth, run `! claude` to
log in and retry.
