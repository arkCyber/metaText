# meta-text-proto

The bottom layer of the [metaText](https://github.com/arkCyber/metaText)
workspace: the *wire* contract and the vocabulary every other layer names.

- `ipc::protocol` — the versioned request / reply / event vocabulary
  (`PROTOCOL_VERSION` 9, serving `3..=9`), internally tagged for `serde`
- `ipc::framing` — bounded length-prefixed framing for stream transports
  (`FRAME_HEADER_LEN` 4 bytes, `MAX_FRAME_LEN` 1 MiB)
- `ipc::validation` — boundary validation of every front-end supplied value
- `error`, `types`, `utils` — the shared error type, domain types and helpers

It depends on nothing else in the workspace, which is what makes the layering of
[`docs/ARCHITECTURE.md`](https://github.com/arkCyber/metaText/blob/main/docs/ARCHITECTURE.md)
§2.1 mechanical: the wire contract cannot grow a dependency on a transport or a
database behind anyone's back.

Part of the metaText workspace (`meta-text`, `meta-text-cli`, `meta-text-tui`,
`meta-text-core`, `meta-text-backend`). License: MIT.
