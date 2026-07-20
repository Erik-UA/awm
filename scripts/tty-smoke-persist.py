#!/usr/bin/env python3
"""Phase-2 smoke: session save/restore across restarts, in a REAL PTY.

Run 1: create a 'web' project (Ctrl+n) and spawn an agent into it (Ctrl+p), then
quit (q) — which persists the session. Run 2: relaunch with the SAME
XDG_STATE_HOME and verify the projects and their panes came back.

Requires: python3 + `pyte`. Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke-persist.py [path-to-awm-binary]
"""
import os, pty, select, struct, fcntl, termios, time, subprocess, sys, tempfile, shutil
import pyte

ROWS, COLS = 30, 110
here = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(here, ".."))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "awm")
STATE = tempfile.mkdtemp(prefix="awm-smoke-state-")

fails = []
def check(cond, msg):
    print(("  OK  " if cond else " FAIL ") + msg)
    if not cond: fails.append(msg)

def run(args, driver, settle=2.0):
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
                try: data = os.read(master, 65536)
                except OSError: return
                if data: stream.feed(data.decode("utf-8", "replace"))
    pump(settle)
    driver(master, pump, screen)
    try: proc.wait(timeout=5)
    except Exception: proc.kill()
    return screen

# ---- Run 1: build a session, then quit (persist) ----------------------------
def build(master, pump, screen):
    os.write(master, b"\x0e"); pump(0.4)          # Ctrl+n
    os.write(master, b"web"); pump(0.2)
    os.write(master, b"\r"); pump(0.5)            # create + switch to 'web'
    os.write(master, b"\x10"); pump(0.3)          # Ctrl+p spawn prompt
    os.write(master, b"scan"); pump(0.2)
    os.write(master, b"\r"); pump(1.5)            # spawn a mock on 'web'
    os.write(master, b"q"); pump(1.0)             # quit -> save_session

print("===== RUN 1: create 'web' + agent, then quit =====")
run(["--mock", "--mock", "--fresh"], build)
saved = os.path.join(STATE, "awm", "session.json")
check(os.path.exists(saved), f"session.json written to {saved}")
if os.path.exists(saved):
    import json
    data = json.load(open(saved))
    names = [p["name"] for p in data["projects"]]
    check("web" in names, f"saved projects include 'web' (got {names})")
    check(len(data["agents"]) >= 3, f"saved >=3 agent panes (got {len(data['agents'])})")

# ---- Run 2: relaunch (no --fresh) -> restore --------------------------------
print("\n===== RUN 2: relaunch, expect restore =====")
def inspect(master, pump, screen):
    pump(1.0)  # keep reading frames so the tab bar is fully painted

screen2 = run([], inspect, settle=2.0)  # no --fresh -> should restore
body = "\n".join(l.rstrip() for l in screen2.display)
print("TAB:", screen2.display[0].rstrip())
for l in screen2.display[:6]: print("  ", l.rstrip())
# The label `[2:web` appears only in the top tab bar — scan the whole frame so
# the check is robust to a mid-draw capture race.
check("[2:web" in body, "restored tab bar shows [2:web]")
# Cycle to web and confirm its restored pane is present.
def go_web(master, pump, screen):
    os.write(master, b"\x0f"); pump(0.6)          # Ctrl+o -> next project (web)
screen3 = run([], go_web, settle=1.5)
webbody = "\n".join(l.rstrip() for l in screen3.display)
check("spawned" in webbody or "scan" in webbody or "session started" in webbody,
      "web screen restored its agent pane")

shutil.rmtree(STATE, ignore_errors=True)
print(f"\nfailures={len(fails)}")
sys.exit(1 if fails else 0)
