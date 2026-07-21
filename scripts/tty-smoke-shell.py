#!/usr/bin/env python3
"""Shell-console pane smoke: open a shell, run a command, escape, quit.

Ctrl+g opens an interactive shell pane in the active project's folder and focuses
it. Focused, keystrokes pass straight through: we send `echo <marker>` + Enter and
confirm the output lands in the pane. `Ctrl+b` (prefix) then `q` escapes passthrough
and quits awm cleanly (exit 0). Driven in a real PTY.

Requires python3 + pyte. Build first: `cargo build -p awm`.
Usage: python3 scripts/tty-smoke-shell.py [path-to-awm-binary]
"""
import os, pty, select, struct, fcntl, termios, time, subprocess, sys, tempfile, shutil
import pyte

ROWS, COLS = 30, 110
here = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.normpath(os.path.join(here, ".."))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "target", "debug", "awm")

CTRL_G, CTRL_B, ENTER = b"\x07", b"\x02", b"\r"
MARKER = "shellmarker_ok_42"

fails = []
def check(cond, msg):
    print(("  OK  " if cond else " FAIL ") + msg)
    if not cond:
        fails.append(msg)


def body(sc):
    return "\n".join(l.rstrip() for l in sc.display)


def run(state, args, driver, quit_prefixed):
    """Launch awm in a PTY, run `driver(write, pump, screen)`, then quit."""
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", XDG_STATE_HOME=state)
    proc = subprocess.Popen([BIN] + args, stdin=s, stdout=s, stderr=s,
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
    driver(lambda b: os.write(m, b), pump, screen)
    # If a shell may be focused, escape passthrough with the Ctrl+b prefix first.
    if quit_prefixed:
        os.write(m, CTRL_B); pump(0.3)
    os.write(m, b"q"); pump(0.6)
    try:
        code = proc.wait(timeout=5)
    except Exception:
        proc.kill(); code = -1
    return code, body(screen)


def main():
    state = tempfile.mkdtemp(prefix="awm-shell-")

    # --- Session 1: open a shell, run a command, confirm the output renders. ---
    def drive1(w, pump, sc):
        w(CTRL_G); pump(1.2)                       # open shell pane, bash starts
        w(b"echo " + MARKER.encode() + ENTER); pump(1.2)
    code1, out1 = run(state, ["--mock", "--fresh"], drive1, quit_prefixed=True)
    check(MARKER in out1, f"shell command output visible in the pane ({MARKER!r})")
    check(code1 == 0, f"awm quit cleanly after Ctrl+b q (exit {code1})")

    # --- Session 2: restart WITHOUT --fresh; the shell pane is re-spawned. ---
    def drive2(w, pump, sc):
        pump(1.0)                                  # let restore + fresh bash settle
    code2, out2 = run(state, ["--mock"], drive2, quit_prefixed=True)
    check("shell" in out2, "restored session re-spawns the shell pane (title 'shell')")
    check(code2 == 0, f"awm quit cleanly on the restored session (exit {code2})")

    if fails:
        print("\nsession 1 screen:\n" + out1)
        print("\nsession 2 screen:\n" + out2)
    shutil.rmtree(state, ignore_errors=True)


if __name__ == "__main__":
    if not os.path.exists(BIN):
        print(f"missing binary: {BIN}\nbuild first: cargo build -p awm"); sys.exit(2)
    main()
    print()
    if fails:
        print(f"FAILED ({len(fails)}): " + "; ".join(fails)); sys.exit(1)
    print("shell smoke passed"); sys.exit(0)
