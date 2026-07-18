#!/usr/bin/env python3
"""Persistent multi-turn mock, like real claude: emits a `result` after EVERY
turn but STAYS ALIVE (reads the next user message) until stdin closes or "bye".
Exercises awm's persistent path — the per-turn result must NOT terminalize the
agent (it maps to TurnEnded)."""
import sys, json


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock-convo", "session_id": "convo-1"})

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
        emit({"type": "assistant", "message": {"role": "assistant",
              "content": [{"type": "text", "text": "goodbye"}]}})
        emit({"type": "result", "subtype": "success", "is_error": False, "result": "bye"})
        break
    emit({"type": "assistant", "message": {"role": "assistant",
          "content": [{"type": "text", "text": f"echo: {text}"}],
          "usage": {"input_tokens": 3, "output_tokens": 2}}})
    # A `result` after each turn — but the process keeps reading stdin.
    emit({"type": "result", "subtype": "success", "is_error": False, "result": "ok",
          "usage": {"input_tokens": 3, "output_tokens": 2}})
