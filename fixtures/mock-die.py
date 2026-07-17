#!/usr/bin/env python3
"""Mock agent that CRASHES mid-work: emits init + a tool_use, then exits without
a `result` — simulating a killed/dead agent process. awm must mark it Failed
rather than leaving it stuck Working."""
import sys, json


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock", "session_id": "mock-die"})
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "tool_use", "id": "toolu_1", "name": "Bash",
     "input": {"command": "sleep 100"}}]}})
# Exit abruptly with no `result` line — the reader sees EOF while Working.
sys.exit(3)
