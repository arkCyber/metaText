# meta-text-core

The headless service of the [metaText](https://github.com/arkCyber/metaText)
workspace: everything a user interface is allowed to see.

- `ipc::core` — `CoreService` (the actor that owns configuration, crypto,
  storage, transport, contacts and statistics) and `CoreHandle` (its bounded
  command queue plus broadcast event stream)
- `ipc::client` — the `CoreClient` contract with an in-process (`LocalClient`)
  and a TCP (`RemoteClient`) implementation
- `ipc::server` — the endpoint that exposes the actor to out-of-process UIs,
  with the handshake, rate limit and queue bounds of §3.7
- `config`, `cli`, `logging` — re-exported from `meta-text-backend`, because a
  caller configures the service with them

The subsystems (`crypto`, `database`, `network`, `transport`, `tox`) live one
crate below and are deliberately **not** re-exported here. A front-end that
depends on this crate cannot name a key, a row or a socket — the boundary of
[`docs/ARCHITECTURE.md`](https://github.com/arkCyber/metaText/blob/main/docs/ARCHITECTURE.md)
§2.1 is enforced by Cargo, not by review.

Part of the metaText workspace. License: MIT.
