## Task: awm-pty — PTY session management

**Context.** Implement the PTY layer behind the frozen API in
`crates/awm-pty/src/lib.rs` (`CommandSpec`, `PtySession`). Use `portable-pty`
(already a dep). No proto types change. See `awm-proto` for the wider contract.

**Scope.** Only `crates/awm-pty/`. Do not touch `awm-proto` or any other crate.

**Acceptance (make green):**
- `crates/awm-pty/tests/spawn.rs` — `spawn_echo_reads_output_and_exits_zero`.
- Implement: spawn cmd/args/cwd/env in a PTY; a bounded ring buffer keeping the
  last `ring_lines` lines (`tail(n)`); `resize(rows, cols)`; `wait() -> exit code`;
  `kill()`. Add your own unit tests for resize and ring-buffer eviction.

**Don't:** touch `awm-proto`; spawn a live `claude`; add global mutable state.

**Notes.** Keep MSRV 1.75 / edition 2021. If a new dep bumps MSRV, pin it
(`cargo update <crate> --precise <ver>`) and note it. `cargo build && cargo test`
before every commit; conventional commits, one step per commit.
