# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Project scaffolding for GitHub: MIT license, contribution guide, code of
  conduct, security policy, issue/PR templates and CI workflows.

## [0.4.0] - 2024-01-15

### Added

- Rust rewrite of the original C `metaText`/toxic prototype.
- End-to-end message encryption with ChaCha20-Poly1305 and Argon2 key
  derivation; peers sharing a `--passphrase` derive the same session key.
- Encrypted peer-to-peer messaging over TCP with the
  `kind || u32 length || payload` framing protocol and per-message
  acknowledgements.
- Slash-command REPL and a full screen `crossterm` + `tui` interface.
- `run` subcommand as an explicit spelling of `--mode cli`.
- Optional durable message history backed by SQLite (`sqlite` feature).
- TOML configuration with CLI overrides and a persisted session file.
- Offline message queue (32 messages, five minute expiry) flushed when a peer
  reconnects.
- Connection supervisor that retries desired peers and re-establishes dropped
  connections automatically.

[Unreleased]: https://github.com/arksong/meta-text-rust/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/arksong/meta-text-rust/releases/tag/v0.4.0
