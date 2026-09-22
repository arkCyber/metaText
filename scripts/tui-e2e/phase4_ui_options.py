#!/usr/bin/env python3
"""Phase 4: the `[ui]` settings reach the interface and are honoured.

The configuration is policy and the presentation layer is written against the core
protocol, so the two meet in the composition root. These checks run the real binary
with a real configuration and ask the *terminal* what changed: a captured mouse, a
coloured or plain frame, a pane that follows new output or stays where it is.
"""

import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import (  # noqa: E402
    CONFIG, PGDN, PGDN_WHEEL, PGUP_WHEEL, ROOT, TuiSession, check, expect, report,
)

# SGR sequences, which is where a colour would be set. A cursor position looks
# similar (`\x1b[30;5H`) but does not end in `m`, so the final byte is part of the
# pattern: counting positions as colours is exactly the false positive that made a
# plain frame look coloured.
SGR = re.compile(r"\x1b\[([0-9;]*)m")
MOUSE_CAPTURE = "\x1b[?1000h"


def colour_codes(raw: str) -> int:
    """How many SGR sequences in `raw` set a colour.

    `39` and `49` are the *default* foreground and background: a frame without any
    style still resets to them, so they are not colours.
    """
    count = 0
    for params in SGR.findall(raw):
        for part in params.split(";"):
            value = int(part) if part.isdigit() else 0
            if 30 <= value <= 38 or 40 <= value <= 48 or 90 <= value <= 97:
                count += 1
                break
    return count


def config_with(**overrides) -> str:
    """The repository configuration with the given `[ui]` keys replaced."""
    with open(CONFIG, encoding="utf-8") as handle:
        text = handle.read()

    for key, value in overrides.items():
        if isinstance(value, bool):
            rendered = "true" if value else "false"
        else:
            # The reference file quotes its strings with single quotes, and TOML
            # needs a string to be quoted at all.
            rendered = f"'{value}'"
        text, count = re.subn(rf"^{key} = .*$", f"{key} = {rendered}", text,
                              count=1, flags=re.M)
        if count != 1:
            raise AssertionError(f"the reference config has no {key!r} key to replace")

    # The values are part of the name, so two checks that set the same key cannot
    # read each other's file.
    label = "-".join(f"{key}_{value}" for key, value in overrides.items())
    path = f"{ROOT}/phase4-{label}.toml"
    os.makedirs(ROOT, exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    return path


def phase_colours() -> None:
    """`enable_colors = false` draws without colour; the theme picks the palette."""
    coloured = TuiSession("phase4-colour", port="34611", nick="Alice").start()
    coloured_codes = colour_codes(coloured.raw)
    check("the default theme colours the frame", coloured_codes > 0,
          f"colour sequences in the capture: {coloured_codes}")
    check("the header shows the configured app name", "metaText" in coloured.screen(),
          coloured.tail())
    coloured.close()

    plain = TuiSession("phase4-plain", port="34612", nick="Alice",
                       config=config_with(enable_colors=False)).start()
    plain_codes = colour_codes(plain.raw)
    check("`enable_colors = false` draws no colour at all", plain_codes == 0,
          f"colour sequences in the capture: {plain_codes}")
    check("the plain frame still draws the interface",
          "Welcome to Metaverse" in plain.screen(), plain.tail())
    plain.close()

    light = TuiSession("phase4-light", port="34613", nick="Alice",
                       config=config_with(theme="light")).start()
    check("`theme = light` still draws the interface",
          "Welcome to Metaverse" in light.screen(), light.tail())
    check("the light theme is a different palette",
          "48;5;4" in light.raw or "\x1b[44m" in light.raw,
          "the nickname badge uses the blue background of the light palette")
    light.close()


def phase_mouse() -> None:
    """`enable_mouse` captures the wheel, and the wheel scrolls the pane."""
    on = TuiSession("phase4-mouse-on", port="34614", nick="Alice").start()
    check("the default configuration captures the mouse",
          MOUSE_CAPTURE in on.raw, "no mouse capture sequence was sent")

    # Fill the pane so there is something to scroll back to.
    on.line("/help", settle=1.5)
    on.line("/info", settle=1.5)
    check("the pane starts by following the newest row",
          "scrolled" not in on.screen(), on.tail())

    on.send(PGUP_WHEEL, 1.0)
    check("the wheel scrolls the conversation pane",
          "scrolled (PgDn)" in on.screen(), on.tail())

    on.send(PGDN_WHEEL, 1.0)
    check("the wheel scrolls back to the newest row",
          "scrolled" not in on.screen(), on.tail())
    on.close()

    off = TuiSession("phase4-mouse-off", port="34615", nick="Alice",
                     config=config_with(enable_mouse=False)).start()
    check("`enable_mouse = false` leaves the terminal's own mouse alone",
          MOUSE_CAPTURE not in off.raw, "a capture sequence was sent anyway")
    off.close()


def phase_auto_scroll() -> None:
    """`auto_scroll` decides whether the pane follows output as it arrives.

    Two sessions, one setting apart, run the same sequence: fill the log past the
    pane, then ask something short. With the default the answer is on screen; with
    auto-scroll off the view stays where it was, the title says so, and `PgDn`
    brings the answer in.
    """
    following = TuiSession("phase4-scroll-on", port="34616", nick="Alice").start()
    following.line("/help", settle=1.5)
    following.line("/info", settle=1.5)
    following.line("/nick Following", settle=0.5)
    expect(following, "the default pane follows the newest answer",
           "nickname set to 'Following'")
    check("a pane that follows carries no scroll mark",
          "scrolled" not in following.screen(), following.tail())
    following.close()

    staying = TuiSession("phase4-scroll-off", port="34617", nick="Alice",
                         config=config_with(auto_scroll=False)).start()
    check("a pane that does not follow starts at the newest row",
          "scrolled" not in staying.screen(), staying.tail())

    # The log has to be taller than the pane, or "not following" would be
    # unobservable: when everything fits, every row is on screen either way.
    staying.line("/help", settle=1.5)
    staying.line("/info", settle=1.5)
    check("output that arrives while the log is filling stops the pane following",
          "scrolled (PgDn)" in staying.screen(), staying.tail())

    staying.line("/nick Staying", settle=1.0)
    check("the pane stayed where it was instead of following the answer",
          "nickname set to 'Staying'" not in staying.screen(), staying.tail())

    # A row below the window is never drawn at all, so "it reached the log" is
    # asserted by asking for it: `PgDn` moves back to the newest rows.
    staying.send(PGDN, 1.2)
    expect(staying, "PgDn brings the newest rows back", "nickname set to 'Staying'",
           timeout=6.0)
    staying.close()


def main() -> int:
    os.system(f"rm -rf {ROOT}/phase4-*")
    phase_colours()
    phase_mouse()
    phase_auto_scroll()
    return report("phase 4 (ui options)")


if __name__ == "__main__":
    raise SystemExit(main())
