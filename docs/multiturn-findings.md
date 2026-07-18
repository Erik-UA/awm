# Persistent claude multi-turn (no -p) — verified 2026-07-18

Ran `claude --input-format stream-json --output-format stream-json --verbose --permission-prompt-tool stdio --include-partial-messages` (NO -p).

- turn 1 reply: 'TURN-ONE'
- process alive after turn-1 result: True
- turn 2 reply: 'TURN-TWO'

Conclusion: dropping -p keeps the session alive; each turn ends with a `result` but the process persists until stdin closes.
