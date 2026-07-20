#!/usr/bin/env python3
"""Directory-browser smoke: Ctrl+n opens a folder picker; navigating + `s`
creates a project for that folder. Driven in a real PTY.

Requires python3 + pyte. Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke-picker.py [path-to-awm-binary]
"""
import os, pty, select, struct, fcntl, termios, time, subprocess, sys
import pyte

ROWS, COLS = 30, 110
here = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(here, ".."))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "awm")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
proc = subprocess.Popen([BIN, "--mock", "--fresh"], stdin=slave, stdout=slave, stderr=slave,
                        start_new_session=True, env=dict(os.environ, TERM="xterm-256color"),
                        cwd=REPO)
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


def top():
    return screen.display[0].rstrip()


fails = []
def check(cond, msg):
    print(("  OK  " if cond else " FAIL ") + msg)
    if not cond:
        fails.append(msg)


pump(2)
# Open the folder browser at the repo cwd.
os.write(master, b"\x0e"); pump(0.6)  # Ctrl+n
print("=== browser opened ===")
for l in screen.display[:12]:
    print("  ", l.rstrip())
check("prototupe" in body(), "browser title shows the current path")
check("crates/" in body(), "browser lists subdirectories (crates/)")
check("select this folder" in body(), "footer legend is shown")

# Move down off `../` to the first subdir and descend into it.
os.write(master, b"j"); pump(0.2)   # highlight first subdir
os.write(master, b"\r"); pump(0.5)  # Enter -> descend

# Select this (sub)folder as a new project.
os.write(master, b"s"); pump(0.6)
print("=== after select ===", top())
check("[2:" in top(), "a new project tab [2:...] was created and made active")
# The new tab is named after the SUBfolder we navigated into, not the repo root —
# proving navigation + descend worked.
check("[2:prototupe]" not in top(), "the project is a subfolder, not the start dir")

os.write(master, b"q"); pump(1)
try:
    proc.wait(timeout=5)
except Exception:
    proc.kill()
print(f"\nfailures={len(fails)} code={proc.returncode}")
sys.exit(1 if fails else 0)
