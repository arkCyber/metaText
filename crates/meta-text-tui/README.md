# meta-text-tui

The presentation layer of the [metaText](https://github.com/arkCyber/metaText)
workspace, shared by both front-ends:

- `presenter` — maps typed lines and core events onto requests and formats the
  replies (all user-visible wording lives here)
- `text` — static help/banner wording
- `commands` — interactive slash command parsing
- `tui` — the terminal rendering engine and console output routing, including the
  alternate screen, the widgets and the scrollback sink; the `crossterm` /
  `tui` dependencies are behind the `terminal-ui` feature
- `ui::tui` — the full screen front-end

The line oriented REPL in `meta-text-cli` renders through the same presenter and
the same console router, which is why the REPL crate depends on this one and not
the other way round. This crate cannot reach `meta-text-backend`: it is the
compile-time form of "a UI cannot construct a key, write a row or open a socket".

Part of the metaText workspace. License: MIT.
