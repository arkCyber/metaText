#!/usr/bin/env python3
"""Phase 1: startup, layout, the line editor and every documented command.

One interface process carries the whole phase, exactly as one user session would:
the pane answers each command in turn and the screen is read after each one.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import (  # noqa: E402
    BACKSPACE, CTRL_L, CTRL_U, DELETE, DOWN, END, HOME, LEFT, PGDN, PGUP, RIGHT,
    ROOT, TuiSession, UP, ask, check, expect, expect_absent, expect_input,
    expect_input_empty, report,
)

# Every documented command and the wording the presenter answers with. Taken
# from a REPL probe of the same command, so the expectation is the real string.
COMMANDS = [
    ("/help", "metaText commands:"),
    ("/help chat", "Usage: /chat [n|name]"),
    ("/help nick", "/nick"),
    ("/readme", "a Web3 decentralized instant messenger"),
    ("/info", "metaText status"),
    ("/whoami", "fingerprint:"),
    ("/version", "metaText v0.4.0 (encryption: ChaCha20-Poly1305)"),
    ("/uptime", "uptime:"),
    ("/stats", "Runtime statistics:"),
    ("/metrics", "persistent=true"),
    ("/nick", "nickname: Alice"),
    ("/nick Alicia", "nickname set to 'Alicia'"),
    ("/status", "status: Keep on Metaverse"),
    ("/status testing the tui", "status set to 'testing the tui'"),
    ("/list", "no friends yet"),
    ("/add Bob", "added friend #1 : Bob"),
    ("/friends", "1. Bob"),
    ("/add Bob", "already exists"),
    ("/chat 1", "now chatting with #1 Bob"),
    ("/to", "active conversation: #1 Bob"),
    ("/chat nosuchfriend", "no friend matches 'nosuchfriend'"),
    ("/msg", "Usage: /msg <name> <text>"),
    ("/msg Bob hello while offline", "buffered as #"),
    ("/me waves at everyone", "waves at everyone"),
    ("/bin 00ff10", "[binary, 3 B]"),
    ("/bin zznothex", "binary body must be hexadecimal"),
    ("/requests", "friend requests only exist on the Tox transport"),
    ("/peers", "No peers connected yet"),
    ("/connections", "waiting to be delivered"),
    ("/group", "the tcp transport has no groups"),
    ("/groups", "the tcp transport has no groups"),
    ("/group create alpha", "could not create a group"),
    ("/connect not-an-address", "is not a valid host:port address"),
    ("/connect 127.0.0.1:1", "could not reach 127.0.0.1:1"),
    ("/history 5", "stored message(s)"),
    ("/log 3", "stored message(s)"),
    ("/save", "session saved to"),
    ("/unknowncommand foo", "unknown command '/unknowncommand'"),
    ("/HELP", "metaText commands:"),
    ("/SEND Bob alias send", "\u2192 Bob: alias send"),
    ("/ACTION jumps about", "\u2192 Bob: *"),
    ("/hex 0011", "[binary, 2 B]"),
    ("/decline abc", "friend requests only exist"),
    ("/accept abc", "friend requests only exist"),
]


def phase_startup(session: TuiSession) -> None:
    screen = session.screen()
    check("startup: the alternate screen is entered", "\x1b[?1049h" in session.raw)
    check("startup: the welcome banner is drawn", "Welcome to Metaverse" in screen)
    check("startup: the four panes are drawn",
          "friends" in screen and "conversation" in screen and "peers (0)" in screen,
          session.tail())
    check("startup: the header carries identity, crypto and database",
          "enc enabled (ChaCha20-Poly1305)" in screen and "db connected" in screen,
          session.tail())
    check("startup: the key hints are drawn",
          "Enter send" in screen and "Ctrl+C/Q quit" in screen and "PgUp/PgDn scroll" in screen,
          session.tail())
    check("startup: --nick reaches the header", "Alice" in screen, session.tail())
    check("startup: the status block answers like the REPL",
          "metaText status" in screen and "fingerprint" in screen, session.tail())


def phase_editor(session: TuiSession) -> None:
    session.send("abc", 0.2)
    expect_input(session, "editor: typing shows the draft", "abc")

    session.send(LEFT + "X", 0.2)
    expect_input(session, "editor: Left moves the cursor into the line", "abXc")

    session.send(HOME + "Y", 0.2)
    expect_input(session, "editor: Home jumps to the start", "YabXc")

    session.send(END + "Z", 0.2)
    expect_input(session, "editor: End jumps to the end", "YabXcZ")

    session.send(BACKSPACE, 0.2)
    expect_input(session, "editor: Backspace deletes before the cursor", "YabXc")

    session.send(HOME + DELETE, 0.2)
    expect_input(session, "editor: Delete removes the character under the cursor", "abXc")

    session.send(RIGHT + "Q", 0.2)
    expect_input(session, "editor: Right moves the cursor forward", "aQbXc")

    session.send(CTRL_U, 0.2)
    expect_input_empty(session, "editor: Ctrl+U empties the draft")

    session.send(DELETE, 0.2)          # Delete on an empty line must not panic
    expect_input_empty(session, "editor: editing an empty draft is harmless")


def phase_history_and_scroll(session: TuiSession) -> None:
    session.line("/version", settle=0.2)
    expect(session, "history: the command is answered", "metaText v0.4.0")

    session.send(UP, 0.2)
    expect_input(session, "history: Up recalls the previous command", "/version")

    session.send(DOWN, 0.2)
    expect_absent(session, "history: Down walks back", "│> /version")
    session.send(CTRL_U, 0.2)

    # Fill the log first. A pane that already shows every row has nothing to scroll
    # back to, and `PageUp` is then correctly a no-op, so the precondition of the
    # checks below is a log that is taller than the pane.
    session.line("/help", settle=1.5)
    session.line("/info", settle=1.5)

    before = session.screen()
    session.send(PGUP, 0.8)
    scrolled_up = session.screen()
    check("scroll: PgUp shows earlier lines", scrolled_up != before, session.tail())
    check("scroll: the pane title says the pane is scrolled",
          "scrolled (PgDn)" in scrolled_up, session.tail())
    session.send(PGDN, 0.8)
    check("scroll: PgDn returns to the newest line",
          session.screen() != scrolled_up, session.tail())
    check("scroll: following the newest line clears the mark",
          "scrolled (PgDn)" not in session.screen(), session.tail())

    session.line("/info", settle=0.2)
    expect(session, "scrollback: /info fills the pane with the status block",
           "metaText status")
    session.send(CTRL_L, 0.2)
    expect_absent(session, "scrollback: Ctrl+L clears the pane", "metaText status")


def phase_commands(session: TuiSession) -> None:
    for command, expected in COMMANDS:
        ask(session, command, expected)

    # /remove is asserted by what it leaves behind, not by its wording.
    ask(session, "/remove 1", "no friends yet",
        label="command '/remove 1' empties the friend list")


def phase_pane_after_wrapping_lines(session: TuiSession) -> None:
    """Regression guard: a full pane still ends with the newest answer.

    The sequence that used to lose answers: fill the pane with a long block
    (`/metrics`), add lines that wrap (the `/connect` and `/group` errors), then
    ask something short. The pane was handed more rows than it could draw and the
    newest rows - the answer - fell outside it until the log was cleared.
    """
    session.send(CTRL_L, 0.6)
    session.line("/metrics", settle=3.0)
    session.line("/connect not-an-address", settle=2.5)
    session.line("/connect 127.0.0.1:1", settle=2.5)
    session.line("/group create alpha", settle=2.5)
    session.line("/list", settle=2.0)
    session.line("/nick Scrolled", settle=2.5)
    check("a full pane still draws the answer that follows wrapping lines",
          "nickname set to 'Scrolled'" in session.screen(), session.tail())

    session.send(CTRL_L, 0.8)
    session.line("/nick Repainted", settle=2.0)
    check("a cleared pane draws the next answer again",
          "nickname set to 'Repainted'" in session.screen(), session.tail())


def phase_narrow_sidebar() -> None:
    """A sidebar that cannot show every friend says how many are out of sight.

    Run on a short terminal, where four friends do not fit: the list is clipped by
    the widget, and without the marker that is indistinguishable from having four
    friends on screen.
    """
    session = TuiSession("phase1-narrow", port="34599", nick="Alice",
                         rows=14, cols=90).start()
    for friend in ("Ada", "Bob", "Cid", "Dee"):
        session.line(f"/add {friend}", settle=0.8)
    expect(session, "the friend list counts what it cannot show", "more",
           timeout=6.0)
    screen = session.screen()
    check("the friend list marks the entries it cannot show",
          "· +" in screen and "more" in screen, session.tail())
    session.close()


def phase_quit(session: TuiSession) -> None:
    session.line("/clear", settle=0.2)
    expect_absent(session, "command '/clear' clears the scrollback like Ctrl+L",
                  "message(s)")
    session.line("/quit", settle=2.0)
    session.wait_exit(10)
    check("quit: /quit leaves the interface", session.exited)
    check("quit: the alternate screen is left behind", "\x1b[?1049l" in session.raw)
    check("quit: the process exited cleanly", session.status == 0, str(session.status))
    session.close()


def main() -> int:
    os.system(f"rm -rf {ROOT}/phase1")
    session = TuiSession("phase1", port="34590", nick="Alice").start()
    phase_startup(session)
    phase_editor(session)
    phase_history_and_scroll(session)
    phase_commands(session)
    phase_pane_after_wrapping_lines(session)
    phase_narrow_sidebar()
    phase_quit(session)
    return report("phase 1 (startup, editor, commands)")


if __name__ == "__main__":
    raise SystemExit(main())
