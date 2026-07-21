#!/usr/bin/env python3
"""Ground-truth the LIVE `interrupt` control_request against `claude`.

awm's Esc-to-interrupt (crates/awm-pty `Answerer::interrupt`) writes
`{"type":"control_request","request_id":"awm-int","request":{"subtype":"interrupt"}}`.
This probe confirms, against a real persistent claude, that:
  (a) that exact envelope is accepted (a matching control_response comes back),
  (b) the current turn stops (a truncated `result` arrives promptly), and
  (c) the SESSION STAYS ALIVE — a follow-up user turn still gets a reply.

Method mirrors awm's control protocol (crates/awm-pty/src/lib.rs): persistent
claude (NO -p), `initialize`, a long-streaming prompt, interrupt mid-stream.

Uses LIVE claude — run manually, not in CI. Requires an authenticated `claude`.
Usage: python3 scripts/capture-interrupt.py
"""
import json
import os
import select
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, ".."))
OUT = os.path.join(REPO, "docs", "interrupt-raw.json")
DEADLINE_S = 120

CLAUDE_ARGS = [
    "claude",
    "--input-format", "stream-json",
    "--output-format", "stream-json",
    "--verbose",
    "--permission-prompt-tool", "stdio",
    "--include-partial-messages",
]

LONG_PROMPT = (
    "Write a long, detailed ~600-word essay about the history of the Unix "
    "operating system, from Multics through the BSD and System V split. Write it "
    "all out in prose now."
)


def send(proc, obj):
    proc.stdin.write((json.dumps(obj) + "\n").encode())
    proc.stdin.flush()


def main():
    print(f"── capture-interrupt: spawning live claude in {REPO} ──")
    proc = subprocess.Popen(
        CLAUDE_ARGS,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        cwd=REPO, env=dict(os.environ, TERM="dumb"),
    )

    send(proc, {"type": "control_request", "request_id": "cap-init",
                "request": {"subtype": "initialize"}})
    send(proc, {"type": "user", "message": {"role": "user", "content": LONG_PROMPT}})

    transcript = []            # every raw line we see (for the dump)
    sent_interrupt = False
    interrupt_at = None
    result_after_interrupt = None
    interrupt_response = None
    stream_chars = 0
    revived = False
    revive_prompt_sent = False
    deadline = time.time() + DEADLINE_S
    buf = b""

    while time.time() < deadline:
        r, _, _ = select.select([proc.stdout], [], [], 0.5)
        if not r:
            if proc.poll() is not None:
                break
            continue
        chunk = os.read(proc.stdout.fileno(), 65536)
        if not chunk:
            break
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            text = line.decode("utf-8", "replace")
            transcript.append(msg)
            typ = msg.get("type")

            if typ == "result" and msg.get("is_error") and not sent_interrupt:
                print("!! early error result (auth?): " + json.dumps(msg)[:300])

            # Count streamed assistant text; once claude is clearly mid-turn, interrupt.
            if typ in ("stream_event", "assistant") and not sent_interrupt:
                stream_chars += len(text)
                if stream_chars > 400:
                    print("→ mid-stream, sending interrupt …")
                    send(proc, {"type": "control_request", "request_id": "awm-int",
                                "request": {"subtype": "interrupt"}})
                    sent_interrupt = True
                    interrupt_at = time.time()

            # A control_response echoed for our interrupt.
            if typ == "control_response" and sent_interrupt and interrupt_response is None:
                rid = msg.get("response", {}).get("request_id") or msg.get("request_id")
                if rid == "awm-int":
                    interrupt_response = msg
                    print("✓ interrupt control_response: " + json.dumps(msg)[:200])

            # The truncated turn's result.
            if typ == "result" and sent_interrupt and result_after_interrupt is None:
                result_after_interrupt = msg
                dt = time.time() - interrupt_at if interrupt_at else None
                print(f"✓ result after interrupt ({dt:.1f}s): " + json.dumps(msg)[:200])
                # (c) prove the session survives: ask something tiny.
                if not revive_prompt_sent:
                    send(proc, {"type": "user", "message": {"role": "user",
                                "content": "Reply with exactly: STILL-ALIVE"}})
                    revive_prompt_sent = True

            # Confirm the follow-up got answered → session alive.
            if revive_prompt_sent and "STILL-ALIVE" in text and typ in ("assistant", "stream_event"):
                revived = True
                print("✓ session alive after interrupt (follow-up answered)")
                break
        if revived:
            break

    with open(OUT, "w") as f:
        json.dump({
            "interrupt_request_sent": {"type": "control_request", "request_id": "awm-int",
                                       "request": {"subtype": "interrupt"}},
            "interrupt_response": interrupt_response,
            "result_after_interrupt": result_after_interrupt,
            "session_alive_after": revived,
            "transcript_len": len(transcript),
        }, f, indent=2)
    print(f"\n→ wrote {OUT}")

    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        proc.terminate(); proc.wait(timeout=5)
    except Exception:
        proc.kill()

    ok = sent_interrupt and result_after_interrupt is not None
    print("\n── summary ──")
    print(f"   interrupt sent      : {sent_interrupt}")
    print(f"   control_response    : {'yes' if interrupt_response else 'no (may be silent)'}")
    print(f"   turn truncated      : {result_after_interrupt is not None}")
    print(f"   session stayed alive: {revived}")
    if not ok:
        err = proc.stderr.read().decode(errors="replace")[:600] if proc.stderr else ""
        if err.strip():
            print("   stderr tail:\n   " + err.replace("\n", "\n   "))
        sys.exit(1)


if __name__ == "__main__":
    main()
