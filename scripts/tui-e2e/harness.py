#!/usr/bin/env python3
"""A reusable fixture for driving the metaText full screen interface.

The interface is a real application, so it is tested the way a user meets it:
on a pseudo terminal, with keystrokes going in and the alternate screen coming
back out. What the capture contains is the *difference* between frames, so the
stream is replayed into a character grid first (`screen.py`); assertions are
written against the screen, never against the raw bytes.

Usage: import this module from a phase script next to it, e.g.

    from harness import TuiSession, ask, check, report
"""

import fcntl
import os
import pty
import re
import select
import signal
import struct
import termios
import time

import screen

# The suite drives the binary the way a user does, so it needs the workspace
# layout: this file lives in <root>/scripts/tui-e2e/.
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BINARY = os.path.join(REPO_ROOT, "target", "debug", "meta-text")
CONFIG = os.path.join(REPO_ROOT, "config.toml")

RESULTS = []

# Where the sessions keep their data, logs and captured screens. Overridable so a
# run can leave its artifacts somewhere else, e.g. under a CI work directory.
ROOT = os.environ.get("METATEXT_TUI_E2E_ROOT", "/tmp/metatext-tui-e2e")

# Key sequences, spelled once so a test reads as the key a user presses.
ENTER = "\r"
UP = "\x1b[A"
DOWN = "\x1b[B"
RIGHT = "\x1b[C"
LEFT = "\x1b[D"
HOME = "\x1b[H"
END = "\x1b[F"
DELETE = "\x1b[3~"
BACKSPACE = "\x7f"
CTRL_U = "\x15"
CTRL_L = "\x0c"
CTRL_C = "\x03"
PGUP = "\x1b[5~"
PGDN = "\x1b[6~"

# Mouse wheel, in the SGR encoding crossterm's capture enables (button 64/65),
# so a session can drive the wheel the way a real mouse would.
PGUP_WHEEL = "\x1b[<64;10;10M"
PGDN_WHEEL = "\x1b[<65;10;10M"


def check(label: str, ok: bool, detail: str = "") -> bool:
    """Record one observation; the process exit status is derived from these."""
    RESULTS.append(bool(ok))
    print(f"{'PASS' if ok else 'FAIL'}: {label}")
    if detail and not ok:
        print(f"      observed: {detail}")
    return bool(ok)


def ask(session, command: str, needle: str, label: str = "", timeout: float = 10.0):
    """Send a command and wait for its answer to be visible in the pane.

    Nothing is cleared here on purpose. The renderer writes only the cells that
    changed relative to its own previous frame, so a pane that is handed more
    rows than it can draw used to lose its newest lines - exactly the answers
    this helper waits for. Waiting for them on a pane that was *not* cleared is
    what keeps that defect from coming back.
    """
    session.line(command, settle=0.2)
    return expect(session, label or f"command {command!r} answers with {needle!r}",
                  needle, timeout=timeout)


def report(title: str) -> int:
    """Print the tally and return a shell exit status."""
    passed = sum(RESULTS)
    print()
    print(f"=== {title}: {passed}/{len(RESULTS)} checks passed ===")
    return 0 if passed == len(RESULTS) else 1


def expect(session, label: str, needle: str, timeout: float = 8.0):
    """Wait for the interface to show `needle`, then record the outcome.

    A fixed sleep would be a guess: an answer arrives when the actor has served
    the request and the frame has been redrawn. Polling the *screen* until the
    expected text appears is both faster on success and accurate on failure.
    """
    deadline = time.time() + timeout
    while True:
        if needle in session.screen():
            check(label, True)
            return True
        if time.time() > deadline:
            check(label, False, session.tail())
            return False
        session.drain(0.25)


def expect_input(session, label: str, needle: str, timeout: float = 5.0):
    """Wait for the line editor to show `needle` (or to stop showing it)."""
    deadline = time.time() + timeout
    while True:
        row = session.input_row()
        if needle in row:
            check(label, True)
            return True
        if time.time() > deadline:
            check(label, False, row)
            return False
        session.drain(0.25)


def expect_raw(session, label: str, needle: str, timeout: float = 8.0):
    """Wait until the interface *wrote* `needle`, whether or not it is on screen.

    This is what a pane that does not auto-scroll needs: the answer has to reach the
    log without the view moving, so the check is on the stream the interface wrote
    rather than on the rows it happens to be showing.

    Whitespace is squeezed out of both sides before comparing: the interface writes
    only the cells that changed, so the spaces between words are usually not part of
    the stream at all ("nicknamesetto'Moving'").
    """
    target = re.sub(r"\s+", "", needle)
    deadline = time.time() + timeout
    while True:
        if target in re.sub(r"\s+", "", session.raw):
            check(label, True)
            return True
        if time.time() > deadline:
            check(label, False, "the interface never wrote it")
            return False
        session.drain(0.25)


def expect_absent(session, label: str, needle: str, timeout: float = 6.0):
    """Wait for the interface to stop showing `needle` (a pane was cleared)."""
    deadline = time.time() + timeout
    while True:
        if needle not in session.screen():
            check(label, True)
            return True
        if time.time() > deadline:
            check(label, False, session.tail())
            return False
        session.drain(0.25)


def expect_input_empty(session, label: str, timeout: float = 5.0):
    """Wait for the line editor to hold nothing but its prompt."""
    deadline = time.time() + timeout
    while True:
        row = session.input_row().strip()
        if row in (">", ""):
            check(label, True)
            return True
        if time.time() > deadline:
            check(label, False, row)
            return False
        session.drain(0.25)


class TuiSession:
    """One interface process on its own terminal."""

    def __init__(self, name: str, port: str = "34590", nick=None, passphrase=None,
                 extra=(), rows: int = 40, cols: int = 140, config=None):
        self.name = name
        self.dir = f"{ROOT}/{name}"
        self.rows, self.cols = rows, cols
        # A session may run a configuration of its own, which is how the `[ui]`
        # settings are exercised end to end.
        self.config = config or CONFIG
        self.argv = [BINARY, "--mode", "tui", "-c", self.config, "-d", f"{self.dir}/data",
                     "--port", port]
        if nick:
            self.argv += ["--nick", nick]
        if passphrase:
            self.argv += ["--passphrase", passphrase]
        self.argv += list(extra)
        self.captured = bytearray()
        self.pid = None
        self.fd = None
        self.status = None
        self.exited = False

    # -- lifecycle --------------------------------------------------------
    def start(self, settle: float = 6.0) -> "TuiSession":
        os.makedirs(f"{self.dir}/data", exist_ok=True)
        pid, fd = pty.fork()
        if pid == 0:                       # the child owns the terminal
            os.environ["TERM"] = "xterm-256color"
            os.chdir(self.dir)             # `logs/` stays out of the working tree
            os.execv(self.argv[0], self.argv)
            os._exit(127)
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", self.rows, self.cols, 0, 0))
        self.drain(settle)

        # An interface that died at startup leaves an empty screen, and every check
        # written against that screen would pass for the wrong reason: a refused
        # configuration (the process exits with the reason on its pty) or a missing
        # feature has to fail here, loudly.
        if not self.is_alive():
            self.write_artifacts()
            raise RuntimeError(
                f"session {self.name!r} exited during startup "
                f"(status {self.status}): {self.raw.strip().splitlines()[:1]}"
            )
        return self

    def is_alive(self) -> bool:
        """Whether the interface process is still running."""
        if self.pid is None:
            return False
        done, status = os.waitpid(self.pid, os.WNOHANG)
        if done:
            self.status = status
            self.pid = None
            return False
        return True

    def drain(self, seconds: float) -> "TuiSession":
        """Collect output for `seconds` without blocking."""
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.fd], [], [], 0.05)
            if ready:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return self
                if not data:
                    return self
                self.captured.extend(data)
        return self

    def send(self, keys: str, settle: float = 0.6) -> "TuiSession":
        """Send raw keystrokes (already encoded, e.g. `UP`).

        A command such as `/quit` ends the process, so a write after that is
        expected to fail; the session simply stays closed instead of raising.
        """
        if self.fd is None:
            return self
        try:
            os.write(self.fd, keys.encode())
        except OSError:
            self.exited = True
            return self
        return self.drain(settle)

    def line(self, text: str, settle: float = 0.9) -> "TuiSession":
        """Type a line and press Enter."""
        if self.fd is None:
            raise RuntimeError(f"session {self.name!r} is not running: call start() first")
        os.write(self.fd, (text + ENTER).encode())
        return self.drain(settle)

    def close(self) -> int:
        self.send(CTRL_C, 2.5)
        for _ in range(60):
            if self.pid is None:       # it already left on its own (`/quit`)
                break
            try:
                done, status = os.waitpid(self.pid, os.WNOHANG)
            except ChildProcessError:
                break
            if done:
                self.status = status
                break
            time.sleep(0.1)
        if self.pid is not None and self.status is None:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
            self.status = -9
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None
        self.write_artifacts()
        return self.status

    def wait_exit(self, timeout: float = 10.0):
        """Wait for the interface to leave on its own (it was told to quit)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                done, status = os.waitpid(self.pid, os.WNOHANG)
            except ChildProcessError:
                break
            if done:
                self.status = status
                self.pid = None
                break
            self.drain(0.2)
        self.exited = self.pid is None
        return self.status

    # -- inspection -------------------------------------------------------
    @property
    def raw(self) -> str:
        return self.captured.decode("utf-8", "replace")

    def screen(self) -> str:
        """The screen as a user sees it, replayed from every frame written."""
        return screen.render(self.raw, rows=self.rows, cols=self.cols)

    def input_row(self) -> str:
        """The line editor's row, which is where a draft is visible.

        The row carries the pane border on both sides, and the conversation pane
        may itself contain a line starting with `>` (the banner's `>>`), so the
        search runs from the bottom up: the editor is the lowest such row.
        """
        for row in reversed(self.screen().splitlines()):
            stripped = row.strip().strip("\u2502").strip()
            if stripped.startswith(">"):
                return stripped
        return ""

    def tail(self, count: int = 6) -> str:
        """The last few rendered rows, for a failure message."""
        rows = [row.strip() for row in self.screen().splitlines() if row.strip()]
        return " / ".join(rows[-count:])[:300]

    def write_artifacts(self) -> None:
        """Leave the raw stream and the final screen next to the data directory."""
        with open(f"{self.dir}/tui.raw.log", "w", encoding="utf-8") as handle:
            handle.write(self.raw)
        with open(f"{self.dir}/tui.screen.txt", "w", encoding="utf-8") as handle:
            handle.write(self.screen())

