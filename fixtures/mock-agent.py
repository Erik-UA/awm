#!/usr/bin/env python3
"""Deterministic mock agent for Track A's control-channel test — NOT live claude.

Emits stream-json, raises a can_use_tool approval gate, then BLOCKS until it reads
a control_response on stdin. On `allow` it proceeds and exits 0; on anything else
it errors and exits 1. This exercises awm-pty's approve/deny write-path offline.
"""
import sys, json


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock", "session_id": "mock-1"})
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "tool_use", "id": "toolu_1", "name": "Write",
     "input": {"file_path": "/tmp/x", "content": "hi"}}]}})
emit({"type": "control_request", "request_id": "req_1", "request": {
    "subtype": "can_use_tool", "tool_name": "Write",
    "input": {"file_path": "/tmp/x", "content": "hi"}, "tool_use_id": "toolu_1"}})

# Block until the controller answers.
line = sys.stdin.readline()
behavior = None
try:
    resp = json.loads(line)
    # {"type":"control_response","response":{"subtype":"success",
    #   "request_id":"req_1","response":{"behavior":"allow",...}}}
    behavior = resp.get("response", {}).get("response", {}).get("behavior")
except Exception:
    behavior = None

if behavior == "allow":
    emit({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_1",
         "content": "post-approval-tool-result", "is_error": False}]}})
    emit({"type": "result", "subtype": "success", "is_error": False,
          "result": "done", "usage": {"input_tokens": 10, "output_tokens": 5}})
    sys.exit(0)
else:
    emit({"type": "result", "subtype": "error", "is_error": True,
          "result": "denied"})
    sys.exit(1)
