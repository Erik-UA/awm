#!/usr/bin/env python3
"""Capture the LIVE `ExitPlanMode` approval-gate envelope from `claude`.

awm's parser distills a `can_use_tool` request down to `ApprovalCtx` (dropping
`permission_suggestions` / `decision_reason_type`), so to ground the plan-gate
overlay we grab the *raw* envelope directly off the stream-json wire.

Method (mirrors awm's own control protocol — see crates/awm-pty/src/lib.rs):
  1. spawn a persistent `claude` (NO -p) in stream-json in/out mode,
  2. `initialize` handshake, then `set_permission_mode {mode: plan}`,
  3. send a user message that makes claude research + present a plan,
  4. on the `ExitPlanMode` `can_use_tool` control_request: dump the raw envelope
     to docs/exitplanmode-raw.json, print `input.plan`, answer `deny` (keep
     planning), close stdin.

Uses LIVE claude — run manually, not in CI. Requires an authenticated `claude`.
Usage: python3 scripts/capture-plan-gate.py
"""
import json
import os
import select
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(HERE, ".."))
OUT = os.path.join(REPO, "docs", "exitplanmode-raw.json")
DEADLINE_S = 120

CLAUDE_ARGS = [
    "claude",
    "--input-format", "stream-json",
    "--output-format", "stream-json",
    "--verbose",
    "--permission-prompt-tool", "stdio",
    "--include-partial-messages",
]

PLAN_PROMPT = (
    "Make a short plan (do not implement anything) for adding a `--version` flag "
    "to a small Rust CLI: which file to edit and the one function to change. "
    "When your plan is ready, call ExitPlanMode to present it."
)


def send(proc, obj):
    proc.stdin.write((json.dumps(obj) + "\n").encode())
    proc.stdin.flush()


def main():
    print(f"── capture-plan-gate: spawning live claude in {REPO} ──")
    proc = subprocess.Popen(
        CLAUDE_ARGS,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=REPO,
        env=dict(os.environ, TERM="dumb"),
    )

    # 1) handshake  2) plan mode  3) the prompt
    send(proc, {"type": "control_request", "request_id": "cap-init",
                "request": {"subtype": "initialize"}})
    send(proc, {"type": "control_request", "request_id": "cap-mode",
                "request": {"subtype": "set_permission_mode", "mode": "plan"}})
    send(proc, {"type": "user",
                "message": {"role": "user", "content": PLAN_PROMPT}})

    buf = b""
    mode_ok = False
    captured = False
    deadline = time.time() + DEADLINE_S

    while time.time() < deadline and not captured:
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

            # Auth / fatal guard.
            if typ == "result" and msg.get("is_error"):
                print("\n!! claude returned an error result:")
                print("   " + json.dumps(msg)[:400])
                print("   If this is an auth error, run `! claude` to log in, then retry.")

            # Plan-mode echo.
            if typ == "control_response":
                resp = msg.get("response", {}).get("response", {})
                if msg.get("response", {}).get("request_id") == "cap-mode" or "mode" in resp:
                    print(f"   set_permission_mode echoed: {resp.get('mode', resp)}")
                    if resp.get("mode") == "plan":
                        mode_ok = True

            # The gate we came for.
            if typ == "control_request":
                req = msg.get("request", {})
                if req.get("subtype") == "can_use_tool" and req.get("tool_name") == "ExitPlanMode":
                    with open(OUT, "w") as f:
                        json.dump(msg, f, indent=2)
                    print(f"\n✓ captured ExitPlanMode gate → {OUT}")
                    plan = req.get("input", {}).get("plan")
                    print("── input.plan ──")
                    print(plan if plan is not None else "(no `plan` field!)")
                    print("── keys present in the gate `request` ──")
                    print("   " + ", ".join(sorted(req.keys())))
                    # Answer deny → keep planning, then close cleanly.
                    rid = msg.get("request_id")
                    send(proc, {"type": "control_response",
                                "response": {"subtype": "success", "request_id": rid,
                                             "response": {"behavior": "deny",
                                                          "message": "probe: captured, keep planning"}}})
                    captured = True
                    break

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
        print("\n!! no ExitPlanMode gate captured within the deadline.")
        print(f"   plan-mode accepted: {mode_ok}")
        err = proc.stderr.read().decode(errors="replace")[:600] if proc.stderr else ""
        if err.strip():
            print("   stderr tail:\n   " + err.replace("\n", "\n   "))
        sys.exit(1)

    print("\n── done ──")


if __name__ == "__main__":
    main()
