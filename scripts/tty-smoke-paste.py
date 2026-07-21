#!/usr/bin/env python3
"""Regression smoke: pasting a long multi-line plan must NOT collapse the TUI.

Before the fix, a paste arrived as a stream of key events: the first embedded
newline submitted the partial line and dropped out of input mode, then the rest
was executed as hotkeys — a stray `q` quit the whole app. With bracketed paste
enabled + an `Event::Paste` handler, the paste lands in the input buffer as one
chunk and nothing is interpreted as a command.

This drives the real `awm` binary in a pseudo-terminal (like scripts/tty-smoke.py),
opens the spawn prompt (Ctrl+p), sends a BRACKETED PASTE containing newlines and a
`q`, and asserts the process is still alive (didn't quit) and the pasted tail shows
in the bar. Then Enter submits and we quit cleanly.

Requires: python3 + `pyte` (`pip install pyte`). Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke-paste.py [path-to-awm-binary]
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


def screen_text():
    return "\n".join(line for line in screen.display)


def fail(msg):
    show("state at failure")
    print(f"\n!! FAIL: {msg}")
    try:
        proc.terminate()
    except Exception:
        pass
    sys.exit(1)


CTRL_B = b"\x02"  # one-shot shell-escape prefix (next key is an awm hotkey)
CTRL_P = b"\x10"
PASTE_START = b"\x1b[200~"
PASTE_END = b"\x1b[201~"
# A "plan" with newlines AND a `q` (in "quit") — the exact shape that used to
# submit early and then quit the app.
PLAN = (
    "PLAN:\n"
    "step 1 gather context\n"
    "step 2 quit early only if blocked\n"
    "step 3 implement and verify the whole thing end to end"
)

pump(2.0)
show("startup: three mock agents")
if proc.poll() is not None:
    fail("awm exited during startup")

# Open the spawn prompt, then paste the multi-line plan as one bracketed paste.
# `Ctrl+b` first, so this works even if a restored session focuses a shell pane
# (the prefix routes the next key to awm instead of the shell's PTY).
os.write(master, CTRL_B)
pump(0.2)
os.write(master, CTRL_P)
pump(0.4)
os.write(master, PASTE_START + PLAN.encode() + PASTE_END)
pump(0.8)
show("after paste (still in input mode, app alive)")

# THE REGRESSION GUARD: the app must still be running (the `q` in the paste and
# the newlines must NOT have been executed as hotkeys).
if proc.poll() is not None:
    fail(f"awm exited after paste (rc={proc.returncode}) — paste ran as hotkeys")

txt = screen_text()
# The tail of the pasted plan is visible in the 1-row bar, flattened to a single
# line (newlines → spaces) with the trailing cursor block. (For a long paste the
# `spawn agent>` prefix is correctly clipped off the left by `clip_prompt`.)
if "end to end" not in txt:
    fail("pasted tail not visible in the prompt bar — paste didn't reach the input")
if "█" not in txt:  # the █ cursor block the prompt bar appends
    fail("cursor block not shown — not in input mode")

# Enter submits the whole plan; the app keeps running.
os.write(master, b"\r")
pump(0.8)
show("after Enter: plan submitted, app alive")
if proc.poll() is not None:
    fail(f"awm exited after submitting the pasted plan (rc={proc.returncode})")

# Clean quit (Ctrl+b prefix in case a shell pane still holds focus).
os.write(master, CTRL_B)
pump(0.2)
os.write(master, b"q")
pump(1.0)
try:
    proc.wait(timeout=5)
except subprocess.TimeoutExpired:
    fail("awm did not quit on 'q' after the test")

print(f"\n[awm exited with code {proc.returncode}]")
print("PASS: paste did not collapse the app; plan captured as one prompt.")
sys.exit(proc.returncode or 0)
