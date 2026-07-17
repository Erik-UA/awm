#!/usr/bin/env python3
"""Interactive smoke test for the `awm` TUI in a REAL pseudo-terminal.

The interactive binary needs a TTY, so this gives it one (openpty), drives it
with keystrokes, and renders the screen with pyte — a way to exercise / demo the
interactive path where no terminal is attached (CI, sandboxes).

Requires: python3 + `pyte` (`pip install pyte`). Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke.py [path-to-awm-binary]
"""
import os, pty, select, struct, fcntl, termios, time, subprocess, sys
import pyte

ROWS, COLS = 30, 110
here = os.path.dirname(os.path.abspath(__file__))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, "..", "target", "debug", "awm")

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
proc = subprocess.Popen(
    [BIN],  # no args -> three mock agents
    stdin=slave, stdout=slave, stderr=slave,
    start_new_session=True, env=dict(os.environ, TERM="xterm-256color"),
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


def show(label):
    print(f"\n===== {label} =====")
    for line in screen.display:
        print(line.rstrip())


pump(2.0)
show("startup: agents blocked -> urgent promoted to master")

for _ in range(3):
    os.write(master, b"y")   # approve the master (oldest blocked)
    pump(0.6)
show("after 'y' x3: agents approved -> done")

os.write(master, b"q")       # quit
pump(1.0)
proc.wait(timeout=5)
print(f"\n[awm exited with code {proc.returncode}]")
sys.exit(proc.returncode or 0)
