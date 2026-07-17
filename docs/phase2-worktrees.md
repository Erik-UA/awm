# Phase 2 — launching the parallel tracks

`awm-proto` is frozen. The three tracks (A/pty, B/parser, C/tui) are independent —
each touches only its own crate and turns its red acceptance test green. Run each
in an isolated git worktree so agents can't collide.

## Set up worktrees (from the repo root)

```bash
# one branch + worktree per track, all based off the frozen main
git worktree add -b track-a-pty    ../awm-track-a HEAD
git worktree add -b track-b-parser ../awm-track-b HEAD
git worktree add -b track-c-tui    ../awm-track-c HEAD
```

Then launch one Claude Code per worktree, handing it the matching task card:

| Worktree        | Crate         | Task card                        |
|-----------------|---------------|----------------------------------|
| `../awm-track-a`| `awm-pty`     | `docs/tasks/track-a-pty.md`      |
| `../awm-track-b`| `awm-parser`  | `docs/tasks/track-b-parser.md`   |
| `../awm-track-c`| `awm-tui`     | `docs/tasks/track-c-tui.md`      |

## Rules the tracks follow

- Touch only the assigned crate; `awm-proto` is read-only (frozen).
- Done = the crate's acceptance test in `tests/` is green, plus the track's own
  added unit tests, with `cargo build && cargo test` clean.
- MSRV 1.75 / edition 2021; pin any MSRV-bumping dep via `cargo update --precise`.

## Integrate (orchestrator, after all three are green)

```bash
git merge --no-ff track-a-pty track-b-parser track-c-tui   # or one at a time
cargo test --workspace --no-fail-fast                       # expect all green now
git worktree remove ../awm-track-a   # etc.
```

That green full-workspace run is the entry condition for Phase 3 (`awm-core`:
event bus, registry, layout engine, and the urgent → master wiring).
