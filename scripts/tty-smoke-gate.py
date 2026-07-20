#!/usr/bin/env python3
"""Live e2e: awm DISPLAYS and ANSWERS an AskUserQuestion gate from a real claude.

Spawns `awm --claude "<prompt that calls AskUserQuestion>"`, waits for the pane to
block — the inline decision menu auto-shows in the pane — checks it renders the
options + hint, picks the SECOND option (Down, Enter), and verifies the live
agent proceeds acknowledging that choice.

LIVE claude — run manually, not CI. Requires an authenticated `claude` + pyte.
Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke-gate.py [path-to-awm-binary]
"""
import os, pty, select, struct, fcntl, termios, time, subprocess, sys, tempfile
import pyte

ROWS, COLS = 40, 120
here = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(here, ".."))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "awm")
STATE = tempfile.mkdtemp(prefix="awm-gate-")

PROMPT = (
    "Use the AskUserQuestion tool right now with EXACTLY TWO questions in one call. "
    "Question 1: header 'Q1', options with labels exactly 'A1' and 'B1'. "
    "Question 2: header 'Q2', options with labels exactly 'A2' and 'B2'. "
    "Both single-select. Do nothing else first."
)

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
env = dict(os.environ, TERM="xterm-256color", XDG_STATE_HOME=STATE)
proc = subprocess.Popen([BIN, "--claude", PROMPT, "--fresh"],
                        stdin=slave, stdout=slave, stderr=slave,
                        start_new_session=True, env=env, cwd=REPO)
os.close(slave)
screen = pyte.Screen(COLS, ROWS)
stream = pyte.Stream(screen)


def pump(sec):
    end = time.time() + sec
    while time.time() < end:
        r, _, _ = select.select([master], [], [], 0.1)
        if r:
            try:
                data = os.read(master, 65536)
            except OSError:
                return
            if data:
                stream.feed(data.decode("utf-8", "replace"))


def body():
    return "\n".join(l.rstrip() for l in screen.display)


fails = []
def check(cond, msg):
    print(("  OK  " if cond else " FAIL ") + msg)
    if not cond:
        fails.append(msg)


print("===== waiting for the live agent to block on a 2-question AskUserQuestion =====")
pump(30)  # establish session + call the tool + block → inline menu auto-shows
scr = body()
check("BLOCKED" in scr, "pane reached BLOCKED on the gate")
# The inline menu should appear on its own — and show BOTH groups.
check("A1" in scr and "B1" in scr, "group 1 options render inline (A1/B1)")
check("A2" in scr and "B2" in scr, "group 2 options render inline (A2/B2)")
check("Enter send" in scr, "inline menu shows the key hint")

print("===== pick Q1=A1 (default) and Q2=B2 (navigate + Space), then Enter =====")
os.write(master, b" "); pump(0.3)          # Space → pick A1 in group 1 (option 0)
os.write(master, b"\x1b[B"); pump(0.2)     # Down → (Q1,B1)
os.write(master, b"\x1b[B"); pump(0.2)     # Down → (Q2,A2)
os.write(master, b"\x1b[B"); pump(0.2)     # Down → (Q2,B2)
os.write(master, b" "); pump(0.3)          # Space → pick B2 in group 2
os.write(master, b"\r"); pump(25)          # Enter → answer both, read follow-up
final = body()
# Both answers must reach the agent (A1 for Q1, B2 for Q2).
check("A1" in final, "group 1 answered (A1)")
check("B2" in final, "group 2 answered (B2)")

os.write(master, b"q"); pump(1)
try:
    proc.wait(timeout=8)
except Exception:
    proc.kill()

print("\n----- final screen -----")
print(final)
print(f"\nfailures={len(fails)}")
sys.exit(1 if fails else 0)
