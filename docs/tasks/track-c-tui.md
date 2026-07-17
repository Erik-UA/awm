## Task: awm-tui — Ratatui rendering + keymap

**Context.** Implement `AwmTui::render` and `keymap::map_key`
(`crates/awm-tui/src/`) against the frozen `Renderer` trait and `AgentView` DTO in
`awm-proto`. The renderer is a pure function of `&[AgentView]` + `LayoutCmd`; it
holds no layout policy of its own.

**Scope.** Only `crates/awm-tui/`. Do not touch `awm-proto`.

**Acceptance (make green):**
- `crates/awm-tui/tests/snapshot.rs` — `master_stack_promotes_urgent_agent`
  (run `cargo insta review` to accept the first snapshot).
- Render master + side-stack from a `Vec<AgentView>`; a status bar per agent
  (name / tag / state / tokens); urgent agents (`AgentView::is_urgent`) visibly
  highlighted. Also support `Monocle` and `Triage` layouts.
- `map_key`: `Mod+j/k` focus, `Mod+Enter` zoom master, `Mod+m` monocle,
  `Mod+1..9` tags, `Mod+p` spawn, `y/n/e` on urgent → Approve/Deny/EditInline.
  Add `insta`/unit snapshots for monocle + triage and key→Action unit tests.

**Don't:** touch `awm-proto`; read real PTYs or invoke `claude` (use mock views).

**Notes.** MSRV 1.75 / edition 2021, ratatui 0.26 API. `cargo build && cargo test`
before commits.
