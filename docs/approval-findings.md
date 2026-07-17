# Approval-detection findings (the killer feature's core risk)

**Date:** 2026-07-17 · **Claude Code:** 2.1.212 · **Model:** claude-opus-4-8[1m]
**Evidence:** `docs/approval-capture.jsonl` (sanitized; raw + driver in the
session scratchpad `record_approval.py` / `record_v2.py`).

## Goal

Confirm how a tool **approval gate** surfaces in `claude -p --output-format
stream-json`, because awm's headline feature (urgent → master) triggers on it.

## Method

Drove `claude` in `--input-format stream-json --output-format stream-json` and
asked it to run a Bash command, acting as the stdin controller. Three runs:
`--permission-mode default`, `--permission-mode manual`, and an explicit
`initialize` handshake + runtime `set_permission_mode: manual`.

## Findings

1. **Benign tools auto-approve; nothing hits the stream.** `echo …` via Bash ran
   with no gate; `result.permission_denials == []`. **Approvals are not passive
   stdout events.**
2. **`--permission-mode manual` is ignored** in this path: `init` still reports
   `"permissionMode":"default"`.
3. **Runtime `set_permission_mode: "manual"` is coerced** → the CLI answers
   `{"mode":"default"}`.
4. **The `initialize` control handshake works** (CLI replies with a
   `control_response` carrying the slash-command list), but declaring
   `capabilities.canUseTool` did **not** route `can_use_tool` to our stdin.
5. **Confirmed passive schema** (matches our synthetic fixtures): `system/init` →
   `assistant` blocks (`thinking` | `text` | `tool_use`) → `user` (`tool_result`)
   → `result`. Also a new top-level **`rate_limit_event`** type → must map to
   `Noise` (our robustness contract already covers unknown types).

## Consequences for awm

- **`awm-parser` (Track B) will never emit `ApprovalRequested` from stdout.**
  Keep it a pure passive normalizer.
- **Add a control-channel handler** (Track A/PTY or a new integration concern):
  run the agent via stream-json *input* mode, complete the Agent SDK controller
  negotiation, receive `can_use_tool`, answer with `control_response`, and feed
  `ApprovalRequested`/`ApprovalResolved` into the shared `EventSource`.
- **`awm-proto` is unaffected** — `EventSource` already abstracts the source, so
  the frozen contract holds. (Good outcome for the contracts-first bet.)

## RESOLVED — the `can_use_tool` wire shape (Agent SDK spike, 2026-07-17)

Ran an Agent SDK (`claude-agent-sdk` 0.2.121) spike with a `can_use_tool`
callback, monkeypatching `Query._handle_control_request` to dump the raw
envelope. A `Write` to `/tmp/spike.txt` (outside cwd) gated and produced the
authoritative request (raw in `docs/canusetool-raw.json`):

```json
{"type":"control_request","request_id":"41dbce5f-…","request":{
  "subtype":"can_use_tool","tool_name":"Write","display_name":"Write",
  "input":{"file_path":"/tmp/spike.txt","content":"hello\n"},
  "description":"/tmp/spike.txt",
  "permission_suggestions":[
    {"type":"setMode","mode":"acceptEdits","destination":"session"},
    {"type":"addDirectories","directories":["/tmp"],"destination":"session"}],
  "decision_reason":"Path is outside allowed working directories",
  "decision_reason_type":"workingDir",
  "tool_use_id":"toolu_01Nos3…"}}
```

Controller answers (SDK source `_internal/query.py`):

```json
// allow
{"type":"control_response","response":{"subtype":"success","request_id":"<id>",
  "response":{"behavior":"allow","updatedInput":{...}}}}
// deny
{"type":"control_response","response":{"subtype":"success","request_id":"<id>",
  "response":{"behavior":"deny","message":"<why>"}}}
```

Extra confirmations:
- A gate fires **only for non-auto-approved calls** — `echo` and in-cwd ops
  auto-approve and never reach the callback (even with `can_use_tool` registered).
  Our gate triggered because the path was outside the working directory.
- The tool_use ALSO appears in the passive stream (`AssistantMessage/ToolUseBlock`)
  before the gate; correlate via `tool_use_id`.
- Denied tools are additionally listed in `result.permission_denials[]`.

`awm-proto::ApprovalCtx` was updated (pre-freeze) to carry `tool`, `input`,
`request_id`, `tool_use_id`, `description`, `decision_reason`. `fixtures/approval.jsonl`
now uses this confirmed shape.
