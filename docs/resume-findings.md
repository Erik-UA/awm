# `claude --resume <session_id>` in stream-json input mode — verified 2026-07-20

Ground-truthed against **Claude Code 2.1.215** via the headless spike
`awm --probe-resume` (see `crates/awm/src/main.rs::run_probe_resume`).

## Method
- **Turn 1** — spawn a fresh persistent session:
  `claude --input-format stream-json --output-format stream-json --verbose
  --permission-prompt-tool stdio --include-partial-messages`.
  Send "Remember this secret code: BANANA47…", capture the `session_id` from the
  `init` line (now surfaced on `AgentInfo.session_id`), then kill the process.
- **Turn 2** — spawn a NEW process with the SAME flags **plus** `--resume <session_id>`.
  Send "What is the secret code I told you earlier?".

## Result (GREEN)
```
captured session_id = 74daba77-dbcd-442a-b4fe-709c77aedce2
turn1 reply: "OK"
turn2 reply: "BANANA47"     ← resumed session recalled the planted secret
```

## Conclusion
`claude --resume <session_id>` **does** continue a persistent conversation in
stream-json *input* mode: the resumed process starts, accepts new stream-json
user messages, and has the full prior context. **Phase-4 live restore is viable
via `--resume` — no fallback (`--continue`/fresh session) needed.**

## Implications for awm
- Persist each root agent's `session_id` (already in the `SessionState` snapshot).
- On restore, re-attach a live process per root pane with `claude --resume <id>`
  (`claude_spec(cwd, Some(&session_id))`), keeping the restored transcript.
- Sub-agent panes share the root process and have no own `session_id` — restored
  as history only; the resumed root re-creates them if it runs sub-agents again.
- Each resume mints a NEW OS process (new pid) but the SAME logical conversation.
