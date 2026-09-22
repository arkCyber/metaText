# meta-text-backend

The subsystem layer of the [metaText](https://github.com/arkCyber/metaText)
workspace. It holds everything the core service drives and nothing that drives
the core service:

- `crypto` — Argon2 key derivation and ChaCha20-Poly1305 authenticated encryption
- `database` — SQL persistence for contacts and message history
- `network` — the peer to peer TCP transport, including the per-pair key agreement
  (`FRAME_EPHEMERAL`, three-DH over the two static keys and one ephemeral per
  connection)
- `identity` — the persisted X25519 key pair an instance announces, and the file
  that keeps it the same across restarts
- `trust` — trust on first use for a peer's announced identity, so a change under
  a known nickname is reported
- `tox` — the optional Tox transport over the system `libtoxcore`
  (`tox-protocol` feature; `build.rs` finds and links the C library)
- `tox_store` — the file that makes an unanswered friend request and an outbox
  payload survive a restart (same feature as `tox`): bounded JSON, keyed by
  public key, written next to the savedata
- `transport` — the abstraction that lets the actor drive either one
- `config`, `logging`, `cli` — configuration, tracing and command line vocabulary

[`docs/ARCHITECTURE.md`](https://github.com/arkCyber/metaText/blob/main/docs/ARCHITECTURE.md)
§2.1 in one sentence: this crate sits directly above `meta-text-proto` and one
crate below `CoreService`, so a subsystem cannot name the service or a front-end.

Part of the metaText workspace. License: MIT.
