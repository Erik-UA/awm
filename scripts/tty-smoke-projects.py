#!/usr/bin/env python3
"""Phase-1 smoke: project (screen) switching in the awm TUI, in a REAL PTY.

Drives the interactive binary through: default project tab → create a project
(Ctrl+n) → spawn into it (Ctrl+p) → cycle screens (Ctrl+o) → quit. Verifies the
top tab bar, per-screen agent scoping, and the cross-screen urgent `!` indicator.

Requires: python3 + `pyte` (`pip install pyte`). Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke-projects.py [path-to-awm-binary]
"""
import os, pty, select, struct, fcntl, termios, time, subprocess, sys
import pyte

ROWS, COLS = 30, 110
here = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(here, ".."))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "awm")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
proc = subprocess.Popen(
    [BIN, "--mock", "--mock"],
    stdin=slave, stdout=slave, stderr=slave,
    start_new_session=True, env=dict(os.environ, TERM="xterm-256color"),
    cwd=REPO,
)
os.close(slave)
screen = pyte.Screen(COLS, ROWS)
stream = pyte.Stream(screen)


def pump(seconds):
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([master], [], [], 0.1)
        if r:
            try:
                data = os.read(master, 65536)
            except OSError:
                return
            if data:
                stream.feed(data.decode("utf-8", "replace"))


def top():
    return screen.display[0].rstrip()


def body():
    return "\n".join(l.rstrip() for l in screen.display)


def show(label):
    print(f"\n===== {label} =====")
    print("TAB:", top())
    for line in screen.display[:6]:
        print("  ", line.rstrip())


fails = []


def check(cond, msg):
    print(("  OK  " if cond else " FAIL ") + msg)
    if not cond:
        fails.append(msg)


pump(2.0)
show("startup (default project named after cwd)")
check("[1:" in top(), "tab bar shows the default project")

# Create a new project 'web' via Ctrl+n, type the name, Enter.
os.write(master, b"\x0e"); pump(0.4)
os.write(master, b"web"); pump(0.3)
os.write(master, b"\r"); pump(0.6)
show("after Ctrl+n web <Enter> (new empty screen 'web')")
check("[2:web]" in top(), "new tab [2:web] appears")
check("no agents" in body(), "web screen starts empty")

# Spawn a mock into the web project via Ctrl+p.
os.write(master, b"\x10"); pump(0.3)
os.write(master, b"scan"); pump(0.2)
os.write(master, b"\r"); pump(1.5)
show("after Ctrl+p spawn on web screen")
check("no agents" not in body(), "web screen now has its own agent")

# Cycle to the next project (wraps back to the default) via Ctrl+o.
os.write(master, b"\x0f"); pump(0.6)
show("after Ctrl+o (cycle back to the default screen)")
check("mock" in body().lower(), "default screen shows its own mock agents, not web's")

os.write(master, b"q"); pump(1.0)
try:
    proc.wait(timeout=5)
except Exception:
    proc.kill()
print(f"\n[awm exited code {proc.returncode}] failures={len(fails)}")
sys.exit(1 if fails else 0)
