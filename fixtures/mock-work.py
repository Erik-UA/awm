#!/usr/bin/env python3
"""Mock agent that does real work: text, a Bash tool call + result, an Edit
(shown as a red/green diff), then a markdown summary. No approval gate —
exercises the Claude-style transcript (tool call ⏺, diff, result ⎿, markdown)
end-to-end."""
import sys, json


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock-work", "session_id": "work-1"})
emit({"type": "assistant", "message": {"role": "assistant",
      "content": [{"type": "text", "text": "I'll list the files."}],
      "usage": {"input_tokens": 100, "output_tokens": 8}}})
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "tool_use", "id": "toolu_1", "name": "Bash",
     "input": {"command": "ls -la", "description": "List files"}}]}})
emit({"type": "user", "message": {"role": "user", "content": [
    {"type": "tool_result", "tool_use_id": "toolu_1",
     "content": "total 8\ndrwxr-xr-x  src\n-rw-r--r--  Cargo.toml", "is_error": False}]}})
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "tool_use", "id": "toolu_2", "name": "Edit",
     "input": {"file_path": "src/main.rs",
               "old_string": "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}",
               "new_string": "fn main() {\n    let x = 2;\n    let y = x + 1;\n    println!(\"{x} {y}\");\n}"}}]}})
emit({"type": "user", "message": {"role": "user", "content": [
    {"type": "tool_result", "tool_use_id": "toolu_2",
     "content": "The file src/main.rs has been updated.", "is_error": False}]}})
emit({"type": "assistant", "message": {"role": "assistant", "content": [
    {"type": "text", "text": "## Summary\n- **2** entries, incl. a `src` directory\n- patched `main.rs`"}],
    "usage": {"input_tokens": 260, "output_tokens": 30}}})
emit({"type": "result", "subtype": "success", "is_error": False,
      "result": "done", "usage": {"input_tokens": 360, "output_tokens": 38}})
