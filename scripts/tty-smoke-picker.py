#!/usr/bin/env python3
"""Directory-browser smoke: prefix filter + Enter-select, and parent-remembers-child.

Run A: Ctrl+n -> type a prefix (list narrows) -> Enter creates a project for the
highlighted match. Run B: filter -> descend (→) -> up (←) and confirm the highlight
returns to the folder we came from. Driven in a real PTY.

Requires python3 + pyte. Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke-picker.py [path-to-awm-binary]
"""
import os, pty, select, struct, fcntl, termios, time, subprocess, sys, tempfile, shutil
import pyte

ROWS, COLS = 30, 110
here = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(here, ".."))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "awm")

UP, DOWN, RIGHT, LEFT = b"\x1b[A", b"\x1b[B", b"\x1b[C", b"\x1b[D"
CTRL_N, ENTER, ESC, BKSP, TAB = b"\x0e", b"\r", b"\x1b", b"\x7f", b"\t"

fails = []
def check(cond, msg):
    print(("  OK  " if cond else " FAIL ") + msg)
    if not cond:
        fails.append(msg)


def session(driver):
    state = tempfile.mkdtemp(prefix="awm-pick-")
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", XDG_STATE_HOME=state)
    proc = subprocess.Popen([BIN, "--mock", "--fresh"], stdin=s, stdout=s, stderr=s,
                            start_new_session=True, env=env, cwd=REPO)
    os.close(s)
    screen = pyte.Screen(COLS, ROWS); stream = pyte.Stream(screen)
    def pump(sec):
        end = time.time() + sec
        while time.time() < end:
            r, _, _ = select.select([m], [], [], 0.1)
            if r:
                try: data = os.read(m, 65536)
                except OSError: return
                if data: stream.feed(data.decode("utf-8", "replace"))
    pump(1.5)
    driver(m, pump, screen)
    os.write(m, b"q"); pump(0.6)
    try: proc.wait(timeout=5)
    except Exception: proc.kill()
    shutil.rmtree(state, ignore_errors=True)
    return screen


def body(sc):
    return "\n".join(l.rstrip() for l in sc.display)


# ---- Run A: prefix filter + Tab selects ------------------------------------
print("===== RUN A: filter by prefix, Tab selects =====")
def run_a(m, pump, sc):
    os.write(m, CTRL_N); pump(0.6)
    os.write(m, b"crat"); pump(0.4)   # filter -> crates/
    b = body(sc)
    check("find: crat" in b, "title shows the active filter")
    check("crates/" in b, "list narrowed to the match (crates/)")
    check("docs/" not in b, "non-matching dirs hidden")
    os.write(m, TAB); pump(0.6)       # Tab selects highlighted -> project
    check("[2:crates]" in sc.display[0], "Tab created project [2:crates]")
session(run_a)

# ---- Run B: Enter navigates INTO folders; up keeps the place ----------------
print("\n===== RUN B: Enter descends (navigation), back-step keeps highlight =====")
def run_b(m, pump, sc):
    os.write(m, CTRL_N); pump(0.6)
    os.write(m, b"crat"); pump(0.4)   # highlight crates/
    os.write(m, ENTER); pump(0.5)     # Enter -> DESCEND into crates/ (not select!)
    inside = body(sc)
    check("/crates" in inside, "Enter navigated into crates/ (no project created)")
    check("[2:" not in sc.display[0], "Enter did NOT create a project")
    os.write(m, LEFT); pump(0.5)      # go back up to the repo
    up = body(sc)
    # The selected row is marked with "> "; it must be on crates/, not the top.
    check("> crates/" in up, "after going up, highlight is back on crates/")
session(run_b)

print(f"\nfailures={len(fails)}")
sys.exit(1 if fails else 0)
