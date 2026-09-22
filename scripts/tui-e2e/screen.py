#!/usr/bin/env python3
"""A minimal terminal screen model, enough to read back what the TUI drew.

The interface redraws the alternate screen by *difference*: only the cells whose
content changed are written, so the raw byte stream is not the screen and plain
substring searches over a stripped stream are unreliable (they find fragments
like `delivered to Bb`). This module replays the escape sequences into a grid —
the same thing a terminal emulator does — so the assertions can be written
against the text a user actually sees.
"""

import unicodedata

CSI = "\x1b["


def _width(char: str) -> int:
    """Columns a character occupies, as a terminal advances the cursor.

    Zero width matters as much as two: `✅` is wide, but the variation selector
    in an emoji sequence (`\u26a0\ufe0f`) occupies no column at all. Getting this
    wrong shifts every later cell of the row, and because the interface writes
    only the cells that changed relative to its own previous frame, a shifted
    replay drifts away from the real screen instead of failing loudly.
    """
    if char in ("\ufe0e", "\ufe0f"):
        return 0
    if unicodedata.category(char) in ("Mn", "Me", "Cf"):
        return 0
    return 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1


class Screen:
    """A character grid fed by an ANSI byte stream."""

    def __init__(self, rows: int = 40, cols: int = 140):
        self.rows = rows
        self.cols = cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = 0
        self.col = 0

    def _put(self, char: str) -> None:
        width = _width(char)
        if width == 0:
            # A combining mark or variation selector belongs to the character
            # before it: it takes no column of its own.
            return
        if 0 <= self.row < self.rows and 0 <= self.col < self.cols:
            self.grid[self.row][self.col] = char
            # The second cell of a wide character is a filler, never a half glyph.
            for offset in range(1, width):
                if self.col + offset < self.cols:
                    self.grid[self.row][self.col + offset] = ""
        self.col = min(self.col + width, self.cols - 1)

    def _csi(self, params: str, final: str) -> None:
        args = [int(part) for part in params.lstrip("?").split(";") if part.isdigit()]
        first = args[0] if args else 1
        if final in "Hf":                      # cursor position, 1-based
            self.row = min((args[0] if args else 1) - 1, self.rows - 1)
            self.col = min((args[1] if len(args) > 1 else 1) - 1, self.cols - 1)
        elif final == "A":
            self.row = max(self.row - first, 0)
        elif final == "B":
            self.row = min(self.row + first, self.rows - 1)
        elif final == "C":
            self.col = min(self.col + first, self.cols - 1)
        elif final == "D":
            self.col = max(self.col - first, 0)
        elif final == "G":
            self.col = min(first - 1, self.cols - 1)
        elif final == "d":
            self.row = min(first - 1, self.rows - 1)
        elif final == "K":                     # erase in line
            mode = args[0] if args else 0
            if mode == 0:
                for index in range(self.col, self.cols):
                    self.grid[self.row][index] = " "
            elif mode == 1:
                for index in range(0, self.col + 1):
                    self.grid[self.row][index] = " "
            else:
                self.grid[self.row] = [" "] * self.cols
        elif final == "J":                     # erase in display
            if (args[0] if args else 0) == 2:
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
        # Everything else (`m`, `h`, `l`, `r`, …) does not move the cursor.

    def feed(self, text: str) -> None:
        """Replay a captured stream."""
        index = 0
        while index < len(text):
            char = text[index]
            if char == "\x1b":
                if text.startswith(CSI, index):
                    end = index + 2
                    while end < len(text) and not ("@" <= text[end] <= "~"):
                        end += 1
                    if end < len(text):
                        self._csi(text[index + 2:end], text[end])
                    index = end + 1
                    continue
                if text.startswith("\x1b]", index):     # OSC: skip to BEL or ST
                    end = index + 2
                    while end < len(text) and text[end] not in ("\x07",):
                        if text.startswith("\x1b\\", end):
                            break
                        end += 1
                    index = end + 2
                    continue
                index += 2                              # single character escape
                continue
            if char == "\r":
                self.col = 0
            elif char == "\n":
                self.row = min(self.row + 1, self.rows - 1)
            elif char == "\b":
                self.col = max(self.col - 1, 0)
            elif char >= " ":
                self._put(char)
            index += 1

    def lines(self) -> list[str]:
        """The grid as text, one string per row (trimmed)."""
        return ["".join(row).rstrip() for row in self.grid]

    def text(self) -> str:
        """The whole screen as text."""
        return "\n".join(self.lines())


def render(raw: str, rows: int = 40, cols: int = 140) -> str:
    """Convenience wrapper: capture bytes in, screen text out."""
    screen = Screen(rows, cols)
    screen.feed(raw)
    return screen.text()
