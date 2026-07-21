#!/usr/bin/env python3
"""Deterministic mock for the interrupt (Esc) path — NOT live claude.

Emits `init` and a `tool_use` so the agent is in a running (`Working`) turn, then
BLOCKS reading stdin. When it receives an `interrupt` control_request it
acknowledges with an assistant `text` block ("turn-interrupted") and a per-turn
`result`, then STAYS ALIVE reading further user messages (echoing them) — proving
the session survives an interrupt. Anything other than an interrupt errors out.
"""
import sys, json


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock-interrupt", "session_id": "int-1"})
# A long-running turn: a tool call streamed in with no result yet → Working.
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "tool_use", "id": "toolu_1", "name": "Bash",
     "input": {"command": "sleep 999"}}]}})

# Block until the controller interrupts the turn.
line = sys.stdin.readline()
subtype = None
try:
    req = json.loads(line)
    subtype = req.get("request", {}).get("subtype")
except Exception:
    subtype = None

if subtype != "interrupt":
    emit({"type": "result", "subtype": "error", "is_error": True,
          "result": "expected interrupt"})
    sys.exit(1)

# Acknowledge the interrupt and end the (truncated) turn — but stay alive.
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "text", "text": "turn-interrupted"}]}})
emit({"type": "result", "subtype": "success", "is_error": False,
      "result": "interrupted", "usage": {"input_tokens": 1, "output_tokens": 1}})

# Persistent session: keep echoing follow-up user turns (proves it lives on).
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if msg.get("type") != "user":
        continue
    content = msg.get("message", {}).get("content", "")
    if isinstance(content, list):
        content = " ".join(b.get("text", "") for b in content if isinstance(b, dict))
    text = str(content).strip()
    if text.lower() in ("bye", "quit", "exit"):
        break
    emit({"type": "assistant", "message": {"role": "assistant",
          "content": [{"type": "text", "text": f"echo: {text}"}]}})
