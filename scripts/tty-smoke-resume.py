#!/usr/bin/env python3
"""Phase-4 human-gate: a LIVE claude session survives an awm restart via auto-resume.

Run 1 spawns `awm --claude "…secret…"`, lets the session establish, then quits —
persisting the agent's session_id. Run 2 relaunches `awm` (same XDG_STATE_HOME);
it restores the pane and re-attaches a live `claude --resume <session_id>`. We then
send a follow-up and verify the resumed model recalls the secret (context restored).

Uses LIVE claude — run manually, not in CI. Requires python3 + pyte + an
authenticated `claude`. Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke-resume.py [path-to-awm-binary]
"""
import os, pty, select, struct, fcntl, termios, time, subprocess, sys, tempfile, shutil, json
import pyte

ROWS, COLS = 40, 120
here = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(here, ".."))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "awm")
STATE = tempfile.mkdtemp(prefix="awm-live-resume-")
SECRET = "KIWI99"


def session(args, driver, settle):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", XDG_STATE_HOME=STATE)
    proc = subprocess.Popen([BIN] + args, stdin=slave, stdout=slave, stderr=slave,
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

    pump(settle)
    driver(master, pump, screen)
    try:
        proc.wait(timeout=8)
    except Exception:
        proc.kill()
    return screen


def body(screen):
    return "\n".join(l.rstrip() for l in screen.display)


fails = []
def check(cond, msg):
    print(("  OK  " if cond else " FAIL ") + msg)
    if not cond:
        fails.append(msg)


print("===== RUN 1: live claude plants a secret, then quit =====")
def run1(master, pump, screen):
    pump(35)  # let claude establish the session + reply
    os.write(master, b"q"); pump(2)
session([f"--claude", f"Remember this secret code: {SECRET}. Reply with just OK."], run1, settle=3)

saved = os.path.join(STATE, "awm", "session.json")
check(os.path.exists(saved), "session.json written")
sid = None
if os.path.exists(saved):
    for a in json.load(open(saved))["agents"]:
        if a.get("session_id"):
            sid = a["session_id"]
    check(sid is not None, f"a claude session_id was persisted (got {sid})")

print("\n===== RUN 2: restart -> auto-resume -> recall the secret =====")
def run2(master, pump, screen):
    pump(12)  # restore + auto-resume + resumed init
    os.write(master, b"i"); pump(1)  # message bar to the focused (claude) pane
    for ch in b"What is the secret code I told you? Reply with just the code.":
        os.write(master, bytes([ch]))
    pump(0.5)
    os.write(master, b"\r"); pump(40)  # send + wait for the resumed reply
scr2 = session([], run2, settle=3)
check(SECRET in body(scr2), f"resumed claude recalled the secret {SECRET}")

shutil.rmtree(STATE, ignore_errors=True)
print(f"\nfailures={len(fails)}")
sys.exit(1 if fails else 0)
