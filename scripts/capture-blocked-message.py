#!/usr/bin/env python3
"""Ground-truth: is a `user` turn honored while a `can_use_tool` gate is OPEN?

awm lets you type a message to an agent that's blocked on approval. The safe
design (crates/awm-core `Engine::send_message` / `answer`) QUEUES that message and
flushes it only once the gate resolves, because a blocked process is waiting for a
`control_response`, not a `user` turn. This probe checks what live claude actually
does if you inject a user message mid-gate — confirming the queue-and-flush design
is the correct one (vs. an immediate send).

Method: persistent claude, a prompt that trips a Write approval gate; on the gate,
DO NOT answer — inject a user message and watch. Then answer (deny) and inject
another, confirming a post-gate message is honored.

Uses LIVE claude — run manually, not in CI. Requires an authenticated `claude`.
Usage: python3 scripts/capture-blocked-message.py
"""
import json
import os
import select
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, ".."))
OUT = os.path.join(REPO, "docs", "blocked-message-raw.json")
DEADLINE_S = 120

CLAUDE_ARGS = [
    "claude",
    "--input-format", "stream-json",
    "--output-format", "stream-json",
    "--verbose",
    "--permission-prompt-tool", "stdio",
    "--include-partial-messages",
]

GATE_PROMPT = (
    "Create a file at /tmp/awm_probe_blocked.txt containing the single word hi. "
    "Use the Write tool."
)
MID_GATE_MSG = "MIDGATE: ignore that; instead just reply with the word PIVOTED."


def send(proc, obj):
    proc.stdin.write((json.dumps(obj) + "\n").encode())
    proc.stdin.flush()


def main():
    print(f"── capture-blocked-message: spawning live claude in {REPO} ──")
    proc = subprocess.Popen(
        CLAUDE_ARGS,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        cwd=REPO, env=dict(os.environ, TERM="dumb"),
    )

    send(proc, {"type": "control_request", "request_id": "cap-init",
                "request": {"subtype": "initialize"}})
    send(proc, {"type": "user", "message": {"role": "user", "content": GATE_PROMPT}})

    at_gate = False
    gate_request_id = None
    injected_at = None
    honored_before_answer = False
    events_after_inject = []
    answered = False
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
            typ = msg.get("type")

            # The Write approval gate.
            if typ == "control_request" and msg.get("request", {}).get("subtype") == "can_use_tool" and not at_gate:
                at_gate = True
                gate_request_id = msg.get("request_id")
                print("→ at Write gate; injecting a user message WITHOUT answering …")
                injected_at = time.time()
                send(proc, {"type": "user", "message": {"role": "user", "content": MID_GATE_MSG}})
                continue

            # Anything claude emits after the injection but before we answer.
            if at_gate and not answered and injected_at is not None:
                events_after_inject.append(typ)
                if "PIVOTED" in text:
                    honored_before_answer = True
                    print("‼ user message honored WHILE gate open (immediate)")
                # Give it a moment, then answer deny so the run can proceed.
                if time.time() - injected_at > 4 and not answered:
                    print("→ no mid-gate pivot; answering deny now …")
                    send(proc, {"type": "control_response", "response": {
                        "subtype": "success", "request_id": gate_request_id,
                        "response": {"behavior": "deny", "message": "probe: pivoting"}}})
                    answered = True

            # After answering, did the injected pivot land?
            if answered and "PIVOTED" in text:
                print("✓ injected message honored AFTER the gate resolved")
                with open(OUT, "w") as f:
                    json.dump({
                        "honored_while_gate_open": honored_before_answer,
                        "gate_request_id": gate_request_id,
                        "events_seen_after_inject_before_answer": events_after_inject,
                    }, f, indent=2)
                print(f"→ wrote {OUT}")
                try:
                    proc.stdin.close()
                except Exception:
                    pass
                proc.terminate()
                print("\n── verdict ──")
                print(f"   honored mid-gate (immediate) : {honored_before_answer}")
                print("   → queue-and-flush is "
                      + ("optional (immediate works)" if honored_before_answer
                         else "REQUIRED (mid-gate user turn not honored)"))
                return

    print("!! deadline hit without a clean pivot; partial data:")
    print(f"   at_gate={at_gate} answered={answered} events_after_inject={events_after_inject}")
    err = proc.stderr.read().decode(errors="replace")[:600] if proc.stderr else ""
    if err.strip():
        print("   stderr tail:\n   " + err.replace("\n", "\n   "))
    try:
        proc.terminate()
    except Exception:
        proc.kill()
    sys.exit(1)


if __name__ == "__main__":
    main()
