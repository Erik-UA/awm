## Task: awm-parser — stream-json → AgentEvent

**Context.** Implement `StreamParser` (`crates/awm-parser/src/lib.rs`) so it turns
raw `claude -p --output-format stream-json` bytes into `awm_proto::AgentEvent`s,
exposed via the `EventSource` trait. The event→AgentEvent mapping is specified in
`fixtures/README.md` — implement exactly that table.

**Scope.** Only `crates/awm-parser/`. Do not touch `awm-proto`.

**Acceptance (make green):**
- `crates/awm-parser/tests/fixtures.rs` — all 5 fixtures
  (normal/approval/error/subagents/garbage).
- `feed(&[u8])` must be robust to chunk boundaries (buffer partial trailing
  lines) and to garbage (unparseable or unknown → `AgentEvent::Noise`, never panic).
- Add a unit test that feeds a fixture split at arbitrary byte offsets and gets
  the same events as feeding it whole.

**⚠️ Approval is NOT a passive stdout event** — proven against CC 2.1.212, see
`docs/approval-findings.md`. The stdout parser must **not** try to detect
approvals; it only maps init/assistant/user/result/unknown. `ApprovalRequested`
/ `ApprovalResolved` come from a separate **control-channel handler** (a distinct
spike/track: run the agent via stream-json *input* mode as the SDK permission
controller). Both sources feed the same `EventSource`, so `awm-proto` is
untouched. The exact `can_use_tool` wire shape is now CONFIRMED (see
`docs/approval-findings.md` / `docs/canusetool-raw.json`) and encoded in
`fixtures/approval.jsonl`. For THIS crate, `tests/fixtures.rs` drives the
approval fixture through the state machine (a valid contract test regardless of
event source).

**Don't:** touch `awm-proto`; invoke a live `claude` from tests (use fixtures).

**Notes.** MSRV 1.75 / edition 2021. `cargo build && cargo test` before commits.
