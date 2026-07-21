#!/usr/bin/env python3
"""Deterministic mock for the "message a blocked agent" path — NOT live claude.

Emits `init` + a `tool_use`, raises a `can_use_tool` approval gate, then BLOCKS
on stdin. The first line it reads must be the `control_response` (allow/deny). On
allow it emits the tool_result + a per-turn `result` and STAYS ALIVE, then loops
echoing any further user messages. This lets a test queue a user message while the
agent is blocked and prove it's delivered (echoed) only after the gate resolves.
"""
import sys, json


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock-gate-chat", "session_id": "gc-1"})
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "tool_use", "id": "toolu_1", "name": "Write",
     "input": {"file_path": "/tmp/x", "content": "hi"}}]}})
emit({"type": "control_request", "request_id": "req_1", "request": {
    "subtype": "can_use_tool", "tool_name": "Write",
    "input": {"file_path": "/tmp/x", "content": "hi"}, "tool_use_id": "toolu_1"}})

# First stdin line: the approval control_response.
line = sys.stdin.readline()
behavior = None
try:
    resp = json.loads(line)
    behavior = resp.get("response", {}).get("response", {}).get("behavior")
except Exception:
    behavior = None

if behavior != "allow":
    emit({"type": "result", "subtype": "error", "is_error": True, "result": "denied"})
    sys.exit(1)

emit({"type": "user", "message": {"role": "user", "content": [
    {"type": "tool_result", "tool_use_id": "toolu_1",
     "content": "post-approval", "is_error": False}]}})
emit({"type": "result", "subtype": "success", "is_error": False,
      "result": "done", "usage": {"input_tokens": 1, "output_tokens": 1}})

# Persistent: echo any user turns delivered after the gate resolved (this is where
# a message queued while blocked lands).
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
