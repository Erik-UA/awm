#!/usr/bin/env python3
"""Mock agent that spawns two sub-agents via the `Agent` tool, the way real
`claude` does: each spawn is a `tool_use` named "Agent" (with a `subagent_type`
input), and each sub-agent's output arrives as nested assistant frames whose
`parent_tool_use_id` (top-level, like the real stream) points back at the spawn.

awm mints a `↳ ...` child pane per spawn and routes the nested output into it.
Deliberately does NOT emit a final `result`: a turn-end would retire the child
panes (see runtime `route()`), so instead we sleep to keep them on screen. Quit
awm with `q` to stop it. Small sleeps pace the spawns so they appear live."""
import sys
import json
import time


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


def spawn(tool_id, description, prompt):
    emit({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": tool_id, "name": "Agent",
         "input": {"description": description, "prompt": prompt,
                   "subagent_type": "general-purpose"}}]}})


def nested(parent_id, text):
    # Real streams carry parent_tool_use_id at the top level of the frame.
    emit({"type": "assistant", "parent_tool_use_id": parent_id,
          "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})


def result_for(tool_id, text):
    emit({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": tool_id, "content": text, "is_error": False}]}})


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock-subagents", "session_id": "subs-1"})
emit({"type": "assistant", "message": {"role": "assistant",
      "content": [{"type": "text", "text": "Dispatching two subagents to run the tests."}],
      "usage": {"input_tokens": 120, "output_tokens": 12}}})
time.sleep(0.6)

# First sub-agent.
spawn("toolu_1", "run parser tests", "cargo test -p awm-parser")
time.sleep(0.6)
nested("toolu_1", "Compiling awm-parser...")
time.sleep(0.5)

# Second sub-agent, in parallel.
spawn("toolu_2", "run core tests", "cargo test -p awm-core")
time.sleep(0.6)
nested("toolu_2", "Compiling awm-core...")
time.sleep(0.5)

nested("toolu_1", "parser: 10 passed; 0 failed")
nested("toolu_2", "core: 13 passed; 0 failed")
time.sleep(0.4)

result_for("toolu_1", "awm-parser: all tests passed (10/10)")
result_for("toolu_2", "awm-core: all tests passed (13/13)")
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "text", "text": "Both subagents finished: parser 10/10, core 13/13."}],
    "usage": {"input_tokens": 640, "output_tokens": 40}}})

# Keep the turn open so the child panes stay visible (no `result`).
while True:
    time.sleep(3600)
