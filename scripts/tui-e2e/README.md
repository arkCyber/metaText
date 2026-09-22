# Terminal suite for the full screen interface

The unit tests cover the pieces of the interface that are pure functions. This
suite covers what only a real terminal can show: the alternate screen, the pane
layout, the line editor, every documented command, and two instances talking to
each other while the interface draws what arrives.

```bash
make tui-e2e                     # builds the binary and runs both phases
# or, phase by phase
python3 scripts/tui-e2e/phase1_core.py
python3 scripts/tui-e2e/phase2_network.py
```

`phase1_core.py` covers startup and layout, the line editor and its key
bindings, the scrollback, and every documented command and alias (including the
refusals and the usage errors, asserted against the wording the presenter really
prints). `phase2_network.py` covers the network side: a second instance connects,
messages cross in both directions with acknowledgements, an offline peer's
message is buffered and delivered when it returns, a peer with another
passphrase is connected but unreadable (asserted through the interface's own log
file), and nickname, friend list and history survive a restart.
`phase4_ui_options.py` covers the `[ui]` section: it writes a configuration per
setting, runs the binary against it, and asks the terminal what changed — whether
the mouse was captured, whether the frame carries any colour, and whether the pane
follows output or stays where the reader left it.

## How a session is driven

Each session is a real process on a pseudo terminal (`pty`), sized like a normal
window, with keystrokes sent as escape sequences and everything the interface
writes captured.

That capture cannot be searched as text. The interface redraws the alternate
screen by *difference*: only the cells that changed relative to its own previous
frame are written, so a stripped capture contains fragments like `delivered to
Bb` and misses whole lines that a terminal shows. `screen.py` therefore replays
the escape stream into a character grid — the same thing a terminal emulator
does — and the assertions are written against the screen. The model was checked
against [`pyte`](https://pypi.org/project/pyte/), an independent VT emulator, on
a real capture: both produced identical rows.

Two details of that replay are worth knowing, because getting either wrong makes
the suite report failures the application does not have:

- a row is written as runs at absolute columns, so cells nobody rewrote keep the
  previous frame's content — the grid has to keep them too, and a line can only
  be read as a whole;
- a wide character occupies two columns while a variation selector occupies
  none. Counting either wrong shifts the rest of the row and every later frame
  on that row, because the interface will not rewrite a cell it believes it
  already filled.

Artifacts (raw capture, rendered screen, data directory, log file of each
session) are kept under `METATEXT_TUI_E2E_ROOT` (`/tmp/metatext-tui-e2e` by
default), which is also the first place to look when a check fails.

## Requirement

The binary has to be built with the `terminal-ui` feature, which `make tui-e2e`
does. A build without it silently falls back to the line oriented REPL, and the
suite then fails at startup instead of testing the interface — `cargo test` in
particular relinks `target/debug/meta-text` without the feature.
