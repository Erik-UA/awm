#!/usr/bin/env python3
"""Multi-turn mock agent — stays alive and holds a conversation. Emits `init`,
then loops reading stream-json user messages from stdin and replying with an
assistant `text` block per message. Exits on EOF (stdin closed) or a "bye"
message. Proves awm's dialogue path (send follow-up → see reply) offline."""
import sys, json


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock-chat", "session_id": "chat-1"})

for line in sys.stdin:                       # one turn per user message
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
    if isinstance(content, list):            # tolerate block form
        content = " ".join(b.get("text", "") for b in content if isinstance(b, dict))
    text = str(content).strip()
    if text.lower() in ("bye", "quit", "exit"):
        emit({"type": "assistant", "message": {"role": "assistant",
              "content": [{"type": "text", "text": "goodbye"}]}})
        emit({"type": "result", "subtype": "success", "is_error": False,
              "result": "ended", "usage": {"input_tokens": 1, "output_tokens": 1}})
        break
    emit({"type": "assistant", "message": {"role": "assistant",
          "content": [{"type": "text", "text": f"echo: {text}"}],
          "usage": {"input_tokens": 3, "output_tokens": 2}}})
