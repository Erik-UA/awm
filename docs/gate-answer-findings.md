# Answering AskUserQuestion / ExitPlanMode gates (live-confirmed)

**Date:** 2026-07-20 · **Claude Code:** 2.1.215
**Harness:** `scripts/capture-askuserquestion.py`, `scripts/capture-plan-gate.py`
**Raw:** `docs/askuserquestion-raw.json`, `docs/exitplanmode-raw.json`

## Why

`GateView` can render a live gate's plan/options, but awm's `Answerer::answer`
hard-codes `updatedInput:{}` on allow, so a *selection* can't be returned. This
note pins the exact wire shapes so Phase 3 can encode the answer.

## AskUserQuestion — a content-selection gate

Gate (`docs/askuserquestion-raw.json`):

```
request keys : subtype, tool_name, display_name, input, tool_use_id,
               requires_user_interaction (= true)
input keys   : questions
questions[]  : { question, header, options[]{label, description}, multiSelect }
```

**Answer (CONFIRMED live):** allow, with `updatedInput` = the original input plus
an `answers` map keyed by the **question text**, valued by the chosen option
**label**:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"<id>",
  "response":{"behavior":"allow",
    "updatedInput":{"questions":[…original…],
      "answers":{"Do you prefer TABS or SPACES for indentation?":"Tabs"}}}}}
```

Sending exactly this made the agent reply *"You chose **Tabs** for indentation.
Noted."* — i.e. it reads `input.answers[<question>]`. So:

- single-select → `answers[question] = "<label>"`.
- multi-select → `answers[question]` = the chosen labels (send an **array** of
  labels; not yet live-confirmed for multi, mark as assumption).
- cancel / no pick → `behavior:"deny"` with a `message`.

## ExitPlanMode — a binary plan gate

Gate carries `input.plan` (markdown) + `planFilePath`, `requires_user_interaction:
true`, and **no** description/decision_reason (see
`docs/approval-stats-findings.md`). Answer is plain allow/deny — no `updatedInput`
needed:

- "Yes, proceed" → `behavior:"allow"` (empty `updatedInput` is fine).
- "No, keep planning" → `behavior:"deny"` with a message.

## Consequences for awm (Phase 3)

- Add `Decision::AllowWith(String)` (pre-serialized `updatedInput` JSON object) to
  `crates/awm-pty/src/lib.rs`; keep plain `Allow` = `{}`.
- Build the AskUserQuestion `updatedInput` in the bin with `serde_json`:
  clone `ctx.input`, insert `answers` from `GateView::chosen()` → labels.
- Everything the answer needs is already in `record(id).pending`
  (`ApprovalCtx.input`, opaque) — **no `awm-proto` / `awm-parser` change**.
