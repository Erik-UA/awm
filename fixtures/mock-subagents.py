#!/usr/bin/env python3
"""Mock agent that spawns two sub-agents via the `Agent` tool, the way real
`claude` does, and drives each of them into an approval gate — reproducing the
"two subagents, two approvals" scenario offline.

Each sub-agent:
  1. is spawned by a `tool_use` named "Agent" (with a `subagent_type` input);
  2. emits an inner `Bash` tool_use as a nested assistant frame whose
     `parent_tool_use_id` (top-level, like the real stream) points at the spawn —
     so awm routes it into that sub-agent's `↳ ...` child pane;
  3. raises a `can_use_tool` control_request for that Bash. As in the real
     protocol the gate envelope has NO parent_tool_use_id — only `request_id` and
     `request.tool_use_id` (= the Bash id). awm correlates the gate to the child
     pane by that tool_use_id, so each approval blocks its OWN window.

Then it sleeps (no `result`) so both gates stay pending — approve them with `y`
one at a time (each is promoted to master via urgent→master). Quit awm with `q`."""
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


def nested_tool(parent_id, tool_id, command):
    # A sub-agent's inner tool_use: nested (top-level parent_tool_use_id) so it
    # routes into the child pane, and its id is what the gate will reference.
    emit({"type": "assistant", "parent_tool_use_id": parent_id,
          "message": {"role": "assistant", "content": [
              {"type": "tool_use", "id": tool_id, "name": "Bash",
               "input": {"command": command}}]}})


def gate(request_id, tool_id, command, agent_id):
    # can_use_tool: the real envelope has NO parent_tool_use_id — only
    # request.tool_use_id (= the inner tool's id, our routing key) and a direct
    # request.agent_id handle. awm correlates by tool_use_id.
    emit({"type": "control_request", "request_id": request_id, "request": {
        "subtype": "can_use_tool", "tool_name": "Bash",
        "input": {"command": command}, "description": command,
        "tool_use_id": tool_id, "agent_id": agent_id}})


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock-subagents", "session_id": "subs-1"})
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "text", "text": "Dispatching two subagents to run the tests."}],
    "usage": {"input_tokens": 120, "output_tokens": 12}}})
time.sleep(0.6)

# First sub-agent: spawn → inner Bash → its approval gate.
spawn("toolu_1", "run parser tests", "cargo test -p awm-parser")
time.sleep(0.6)
nested_tool("toolu_1", "toolu_b1", "cargo test -p awm-parser")
gate("req-1", "toolu_b1", "cargo test -p awm-parser", "agent-aaa1")
time.sleep(0.7)

# Second sub-agent, in parallel: spawn → inner Bash → its approval gate.
spawn("toolu_2", "run core tests", "cargo test -p awm-core")
time.sleep(0.6)
nested_tool("toolu_2", "toolu_b2", "cargo test -p awm-core")
gate("req-2", "toolu_b2", "cargo test -p awm-core", "agent-bbb2")

# The parent ends its turn while the sub-agents keep working in the background
# (a `result` on a persistent session → TurnEnded, not session end). The child
# panes must survive this — they retire only when the process actually exits.
emit({"type": "result", "subtype": "success", "is_error": False,
      "result": "subagents launched", "usage": {"input_tokens": 200, "output_tokens": 20}})

# Stay alive so the child panes (with their pending gates) remain on screen to be
# approved one at a time.
while True:
    time.sleep(3600)
