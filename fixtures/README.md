# Fixtures — recorded stream-json sessions

Newline-delimited `claude -p --output-format stream-json --verbose` output, used
to test `awm-parser` (Track B) **without ever invoking a live `claude`**. Each
`<name>.jsonl` has a sibling `<name>.expected.json` describing the observable
outcome the parser must reproduce.

These are **hand-authored** from the documented event schema (Phase 0 decision:
synthetic, deterministic, offline). To refresh from a real session:
`just record-fixture <name> "<prompt>"`.

## Event → AgentEvent mapping (the contract Track B implements)

| stream-json line                                   | AgentEvent(s)                        |
|----------------------------------------------------|--------------------------------------|
| `type=system, subtype=init`                        | `Started{model, cwd}`                |
| `type=assistant` block `text`                      | `Thinking`                           |
| `type=assistant` block `tool_use`                  | `ToolStarted{name}`                  |
| `type=assistant` `message.usage` present           | `Tokens{input, output}`              |
| `type=user` `tool_result`                          | *(no event — output goes to the PTY buffer)* |
| `type=control_request, subtype=can_use_tool`       | `ApprovalRequested{tool, input, request_id}` |
| `type=control_response` allow/deny                 | `ApprovalResolved{approved}`         |
| `type=result` (`is_error` → `ok`)                  | `Tokens{final}` then `Finished{ok}`  |
| anything unparseable / unknown                     | `Noise`                              |

## `*.expected.json` schema

```json
{
  "note": "human description",
  "states": ["working", "done"],   // distinct consecutive AgentStates after Idle,
                                    // driving parsed events through AgentState::apply,
                                    // with consecutive duplicates collapsed
  "final_state": "done",
  "approvals": [{ "tool": "Bash", "request_id": "req_7" }],
  "final_tokens": { "input": 3400, "output": 200 }
}
```

The Track B acceptance test drives each fixture's parsed events through
`AgentState::apply`, collapses repeats, and asserts `states` / `final_state` /
`approvals` / `final_tokens`. This is cadence-independent: it pins observable
behavior, not the exact number of events emitted.

## ⚠️ Approval is a CONTROL-CHANNEL concern, not a passive stream event

**Ground-truthed against Claude Code 2.1.212** on 2026-07-17 (see
`docs/approval-findings.md` for the raw capture and method). The plan's original
assumption — "approval shows up as a line in the stdout stream" — is **wrong**:

- In headless `claude -p --output-format stream-json`, a benign tool call
  (`echo …` via Bash) is **auto-approved**. Nothing appears in the stream; the
  final `result` has `permission_denials: []`. Approvals are simply not passive
  events on stdout.
- Passing `--permission-mode manual` does **not** engage a gate: `init` still
  reports `"permissionMode":"default"`, and a runtime
  `set_permission_mode: "manual"` control_request is **coerced back to
  `default`** (`response: {"mode":"default"}`).
- The SDK control channel is real and the `initialize` handshake works, but a
  naive `initialize` with `capabilities.canUseTool` did **not** route permission
  decisions to our stdin. Intercepting `can_use_tool` requires the full Agent
  SDK controller negotiation (not just CLI flags).

**Consequence for architecture.** The passive **`awm-parser` never produces
`ApprovalRequested`.** Approval belongs to a separate **control-channel handler**
that runs the agent via stream-json *input* mode, acts as the permission
controller, receives `can_use_tool`, and answers with a `control_response`. That
handler feeds `ApprovalRequested` / `ApprovalResolved` into the same
`EventSource`. The frozen `awm-proto` contract survives unchanged — this is
purely a Phase-2/3 track-boundary correction.

**`approval.jsonl` models the control channel interleaved into the event log.**
Its `control_request` / `control_response` lines now use the **CONFIRMED** wire
shape (captured 2026-07-17 via an Agent SDK `canUseTool` spike — raw evidence in
`docs/canusetool-raw.json`, method in `docs/approval-findings.md`):

```json
{"type":"control_request","request_id":"<id>","request":{
  "subtype":"can_use_tool","tool_name":"Write","display_name":"Write",
  "input":{...},"description":"<summary>",
  "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],
  "decision_reason":"Path is outside allowed working directories",
  "decision_reason_type":"workingDir","tool_use_id":"toolu_..."}}
{"type":"control_response","response":{"subtype":"success","request_id":"<id>",
  "response":{"behavior":"allow","updatedInput":{...}}}}
```

A gate fires only for tool calls the CLI does NOT auto-approve (a call inside cwd,
or `echo`, auto-approves and never reaches the controller). `ApprovalRequested`
maps: `tool_name`→`tool`, `input`→`input`, envelope `request_id`→`request_id`,
plus `tool_use_id`/`description`/`decision_reason`. Denials also appear
post-hoc in `result.permission_denials[]`.
