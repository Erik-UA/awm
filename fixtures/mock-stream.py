#!/usr/bin/env python3
"""Mock agent that STREAMS its reply, like `--include-partial-messages`. Emits
init, a run of stream_event text_delta chunks (with the block start/stop frames
that arrive as Noise), then the complete assistant message, then result. Proves
the window grows the text live and dedups on finalize."""
import sys, json, time


def emit(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()


def stream(evt):
    emit({"type": "stream_event", "uuid": "u", "session_id": "s", "event": evt})


emit({"type": "system", "subtype": "init", "permissionMode": "default",
      "model": "mock-stream", "session_id": "stream-1"})

chunks = ["Hel", "lo ", "world", ", ", "streamed ", "live!"]
stream({"type": "message_start", "message": {"role": "assistant"}})
stream({"type": "content_block_start", "index": 0,
        "content_block": {"type": "text", "text": ""}})
for c in chunks:
    stream({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": c}})
    time.sleep(0.25)   # visible growth between render ticks
stream({"type": "content_block_stop", "index": 0})
stream({"type": "message_stop"})

# The complete message finalizes the streamed line (dedup).
full = "".join(chunks)
emit({"type": "assistant", "message": {"role": "assistant",
      "content": [{"type": "text", "text": full}],
      "usage": {"input_tokens": 20, "output_tokens": 12}}})
emit({"type": "result", "subtype": "success", "is_error": False,
      "result": full, "usage": {"input_tokens": 20, "output_tokens": 12}})
