#!/usr/bin/env python3
"""Capture the LIVE `AskUserQuestion` approval-gate envelope AND confirm how a
controller returns the user's selection.

ExitPlanMode is a binary allow/deny plan gate, but AskUserQuestion is a
*content-selection* gate: the agent expects the chosen option(s) fed back. The
`can_use_tool` → `behavior:"allow"` protocol carries `updatedInput`, and the
AskUserQuestion tool schema exposes an `answers` map (question → chosen label),
so the hypothesis is: **allow with `updatedInput = {...input, "answers": {...}}`**.
This script captures the raw gate and empirically tests that hypothesis by
allowing with a filled `answers` map and printing the follow-up stream.

Method (mirrors awm's control protocol — crates/awm-pty/src/lib.rs):
  1. persistent `claude` (NO -p), stream-json in/out,
  2. `initialize` handshake,
  3. a prompt that makes claude call AskUserQuestion,
  4. on the gate: dump raw envelope → docs/askuserquestion-raw.json, pick the
     first option of each question, ALLOW with `updatedInput.answers`,
  5. read ~20s of follow-up and print it so we can see whether the agent
     proceeded as if the user picked those options.

LIVE claude — run manually. Requires an authenticated `claude`.
Usage: python3 scripts/capture-askuserquestion.py
"""
import json
import os
import select
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, ".."))
OUT = os.path.join(REPO, "docs", "askuserquestion-raw.json")
DEADLINE_S = 120

CLAUDE_ARGS = [
    "claude",
    "--input-format", "stream-json",
    "--output-format", "stream-json",
    "--verbose",
    "--permission-prompt-tool", "stdio",
    "--include-partial-messages",
]

PROMPT = (
    "Use the AskUserQuestion tool right now to ask me a single question: whether "
    "I prefer TABS or SPACES for indentation. Give exactly two options with short "
    "labels. Do not do anything else first."
)


def send(proc, obj):
    proc.stdin.write((json.dumps(obj) + "\n").encode())
    proc.stdin.flush()


def assistant_text(msg):
    """Pull any text/tool_use summary out of an assistant stream message."""
    out = []
    content = msg.get("message", {}).get("content", [])
    if isinstance(content, list):
        for b in content:
            if b.get("type") == "text":
                out.append(b.get("text", ""))
            elif b.get("type") == "tool_use":
                out.append(f"[tool_use {b.get('name')}] {json.dumps(b.get('input', {}))[:200]}")
    return "\n".join(t for t in out if t)


def build_answers(questions):
    """Hypothesis: updatedInput echoes the input plus an `answers` map keyed by
    the question text, valued by the chosen option label (first option here)."""
    answers = {}
    for q in questions:
        opts = q.get("options", [])
        if opts:
            answers[q.get("question", "")] = opts[0].get("label", "")
    return answers


def main():
    print(f"── capture-askuserquestion: spawning live claude in {REPO} ──")
    proc = subprocess.Popen(
        CLAUDE_ARGS,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=REPO,
        env=dict(os.environ, TERM="dumb"),
    )

    send(proc, {"type": "control_request", "request_id": "cap-init",
                "request": {"subtype": "initialize"}})
    send(proc, {"type": "user", "message": {"role": "user", "content": PROMPT}})

    buf = b""
    captured = False
    answered = False
    deadline = time.time() + DEADLINE_S
    answer_until = None

    while time.time() < deadline:
        # After we answer, keep reading a bit to observe the follow-up, then stop.
        if answered and answer_until and time.time() > answer_until:
            break
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
            typ = msg.get("type")

            if typ == "result" and msg.get("is_error"):
                print("\n!! claude error result: " + json.dumps(msg)[:300])
                print("   If this is auth, run `! claude` to log in, then retry.")

            if typ == "assistant" and answered:
                t = assistant_text(msg)
                if t:
                    print("   [follow-up] " + t.replace("\n", "\n   "))

            if typ == "control_request":
                req = msg.get("request", {})
                if req.get("subtype") == "can_use_tool" and req.get("tool_name") == "AskUserQuestion":
                    if not captured:
                        with open(OUT, "w") as f:
                            json.dump(msg, f, indent=2)
                        print(f"\n✓ captured AskUserQuestion gate → {OUT}")
                        inp = req.get("input", {})
                        qs = inp.get("questions", [])
                        print("── request keys ──\n   " + ", ".join(sorted(req.keys())))
                        print("── input keys ──\n   " + ", ".join(sorted(inp.keys())))
                        print(f"── questions ({len(qs)}) ──")
                        print("   " + json.dumps(qs, indent=2).replace("\n", "\n   "))
                        captured = True

                        # Probe: ALLOW with updatedInput carrying an `answers` map.
                        answers = build_answers(qs)
                        updated = dict(inp)
                        updated["answers"] = answers
                        print("── probing allow with updatedInput.answers ──")
                        print("   " + json.dumps(answers))
                        send(proc, {"type": "control_response",
                                    "response": {"subtype": "success",
                                                 "request_id": msg.get("request_id"),
                                                 "response": {"behavior": "allow",
                                                              "updatedInput": updated}}})
                        answered = True
                        answer_until = time.time() + 20
                    continue

    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        proc.terminate()
        proc.wait(timeout=5)
    except Exception:
        proc.kill()

    if not captured:
        print("\n!! no AskUserQuestion gate captured within the deadline.")
        err = proc.stderr.read().decode(errors="replace")[:600] if proc.stderr else ""
        if err.strip():
            print("   stderr tail:\n   " + err.replace("\n", "\n   "))
        sys.exit(1)

    print("\n── done. Inspect the follow-up above: did the agent proceed as if the "
          "user picked the first option? That confirms the updatedInput.answers shape. ──")


if __name__ == "__main__":
    main()
