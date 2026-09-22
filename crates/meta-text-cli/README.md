# meta-text-cli

The line oriented REPL front-end of the
[metaText](https://github.com/arkCyber/metaText) workspace.

It reads stdin on a bounded queue, hands each line to the presenter owned by
`meta-text-tui`, renders core events, and asks the core to shut down on
`/quit`, EOF (Ctrl+D) or Ctrl+C. It holds no domain state: it cannot reach
`meta-text-backend`, so it cannot name a key, a row or a socket.

Part of the metaText workspace (`meta-text`, `meta-text-tui`, `meta-text-core`,
`meta-text-backend`, `meta-text-proto`). License: MIT.
