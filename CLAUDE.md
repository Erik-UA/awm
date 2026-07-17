# awm — agent window manager

TUI multiplexer for Claude agents. Dynamic layout driven by agent state
(dwm model). Killer feature v0.1: the screen rearranges itself when an agent
blocks on an approval gate (**urgent → master**).

## Rules
- Workspace crates: `awm-proto` (contracts, **FROZEN after Phase 1**), `awm-pty`,
  `awm-parser`, `awm-tui`, `awm-core`, `awm` (bin).
- **Touch only your own crate.** Need another crate changed? Stop, describe the
  problem, and wait for the orchestrator. Do not edit `awm-proto`.
- `awm-proto` is the single source of truth for types. Never duplicate its types.
- Every change: `cargo build && cargo test` before committing (fmt/clippy run in
  CI — this dev box has no rustfmt/clippy component).
- parser/tui tests use only `fixtures/` — **never** invoke a live `claude`.
- Async: tokio + mpsc channels. No global mutable state.
- Conventional commits; one logical step per commit.

## Environment pins (important)
- Built against **rustc 1.75** / `edition 2021`. Do not add edition-2024 crates or
  deps that bump MSRV past 1.75. `Cargo.lock` pins the resolved set — keep it committed.
- If `cargo build` fails with "requires rustc 1.85 …", pin the offender:
  `cargo update <crate> --precise <older-version>`.

## Commands
`just test` / `just lint` / `just record-fixture` / `just demo`
(or, without `just`: `make test` / `make build`, and `cargo test`).
