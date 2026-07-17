#!/usr/bin/env python3
"""Mock agent that interleaves GARBAGE into the stream (a truncated line, a
non-JSON log line, an unknown event type) around a normal approval flow. awm must
tolerate all of it (garbage → Noise) and still drive the agent to Done."""
import sys, json


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


def raw(line):
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock", "session_id": "mock-noisy"})
raw('{"type":"assistant","message":{"content":[{"type":"text","tex')  # truncated
raw("a stray log line leaked into the stream")                        # not JSON
raw('{"type":"rate_limit_event","status":"allowed"}')                 # unknown type
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "tool_use", "id": "toolu_1", "name": "Write",
     "input": {"file_path": "/tmp/x", "content": "hi"}}]}})
emit({"type": "control_request", "request_id": "req_1", "request": {
    "subtype": "can_use_tool", "tool_name": "Write",
    "input": {"file_path": "/tmp/x", "content": "hi"}, "tool_use_id": "toolu_1"}})

line = sys.stdin.readline()
behavior = None
try:
    behavior = json.loads(line).get("response", {}).get("response", {}).get("behavior")
except Exception:
    behavior = None

if behavior == "allow":
    raw("another noisy line after approval")
    emit({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_1",
         "content": "ok", "is_error": False}]}})
    emit({"type": "result", "subtype": "success", "is_error": False,
          "result": "done", "usage": {"input_tokens": 5, "output_tokens": 2}})
    sys.exit(0)
else:
    emit({"type": "result", "subtype": "error", "is_error": True, "result": "denied"})
    sys.exit(1)
