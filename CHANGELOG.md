# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Isolated core/UI architecture.** The backend now runs as a headless actor
  (`ipc::core::CoreService` + `CoreHandle`) and the CLI/TUI are pure
  presentation clients of it:
  - `ipc::protocol`: versioned `Request` / `Reply` / `CoreEvent` / `ErrorInfo`
    contract with stable error codes and protocol version negotiation.
  - `ipc::framing`: bounded length-prefixed framing (1 MiB cap validated before
    allocation, clean-EOF detection, truncation reporting).
  - `ipc::client`: `CoreClient` trait with `LocalClient` (in-process) and
    `RemoteClient` (TCP, handshake + per-request deadline).
  - `ipc::server`: token-authenticated TCP endpoint with constant-time token
    comparison, bounded clients/handshake/queues, and remote `Shutdown` refused.
  - `ui::presenter`: shared command dispatch and all user-visible wording;
    `ui::cli` and `ui::tui` drive it over the protocol.
- `--mode core` for a headless backend, plus `--ipc-listen` / `--ipc-token` to
  expose the core protocol to other front-ends.
- The TUI rendering engine (`tui`) is now domain-free: it consumes `TuiInput`
  and renders `TuiInfo`, with no dependency on `AppEvent`.
- `docs/ARCHITECTURE.md`: interface control document, design controls mapped to
  tests, and the audit with closed/open findings.
- Project scaffolding for GitHub: MIT license, contribution guide, code of
  conduct, security policy, issue/PR templates and CI workflows.
- **Boundary validation** (`ipc::validation`): nickname, status, contact
  identifier, note, message body and peer addresses are validated before they
  reach the domain layer, with documented limits (`MAX_NICKNAME_LEN`,
  `MAX_STATUS_LEN`, `MAX_IDENTIFIER_LEN`, `MAX_NOTE_LEN`), character-based
  length counting and control-character rejection. A rejected request has no
  side effects.
- **Full configuration validation** (`AppConfig::problems`): every section is
  checked (names, limits, ports, timeouts, bootstrap addresses, database type,
  crypto algorithm/KDF, log level and file settings). `main` refuses to start on
  an unusable file instead of failing later.
- **Configuration is now honoured**: `network.connection_timeout` drives dial
  timeouts, and the `[logging]` section selects the level, the file destination,
  whether a console layer exists and how many files to keep.
- `CoreEvent::MessageUndecodable` and protocol version **2**: a frame that
  decrypts but is not valid UTF-8 is reported instead of being displayed with
  replacement characters.
- `utils::is_valid_host_port` (host names, IPv4 and bracketed IPv6) and
  `LogLevel::parse`.
- Load, robustness and security tests: concurrent requests, multi-client
  sharing, boundary rejection over the wire, stale protocol versions, oversized
  frames, the handshake timeout, deterministic corpus and one-byte-at-a-time
  framing.
- **Full screen input line editor** (`tui::InputBuffer`): the TUI now supports
  cursor movement (`Left`/`Right`/`Home`/`End`), `Delete`, `Ctrl+U` to clear the
  line, `Ctrl+A`/`Ctrl+E`, a visibly parked terminal cursor and a bounded
  `Up`/`Down` command history (`INPUT_HISTORY_CAPACITY`). Bracketed paste is
  enabled so pasted text arrives as one event instead of a key storm, and
  `Ctrl+L` clears the scrollback. The key → action mapping (`apply_key`) is a
  pure function with unit tests, so it is covered without a TTY.
- `CliArgs::log_level_override` and an explicit `--log-level` precedence, plus
  tests for the TUI key mapping and the input buffer.

### Changed

- `--log-level` / `--debug` now really override `[logging] level`: the effective
  verbosity is resolved as `RUST_LOG` → explicit command line → configuration
  file → built-in default. `CliArgs::log_level` is now `Option<LogLevel>` so
  "no preference" can be distinguished from an explicit request.
- The transport → core event channel is **bounded** (`network::EVENT_INBOX_CAPACITY`)
  and awaited by the transport, so a peer sending faster than the core can
  absorb is throttled instead of growing memory without limit.
- Configured default nickname/status are re-validated inside `CoreService`, so a
  hand-edited configuration cannot inject an invalid value into the domain.
- `NetworkManager::new` takes a bounded `mpsc::Sender<AppEvent>`; the
  `Shared::dispatch` path is asynchronous to apply backpressure.
- `MetaTextApp` (the former all-in-one coordinator) has been replaced by
  `CoreService`; `meta_text::MetaTextApp` is no longer exported.
- Request queues are bounded and apply backpressure instead of growing without
  limit; socket clients each get a bounded output queue.
- Errors crossing the interface are serializable values with severity, instead
  of `anyhow` chains.

### Fixed

- **Logging precedence.** A `[logging] level` value in `config.toml` used to
  shadow the `--log-level` and `--debug` flags, so a scripted session asking for
  `--log-level error` still emitted informational lines on `stdout`. The command
  line now wins over the configuration file (`RUST_LOG` still wins over both).
- The TUI footer and README document the input-editing keys, so the advertised
  shortcuts match what the interface actually accepts.

### Removed

- Direct coupling between the presentation layer and `crypto` / `database` /
  `network`; the front-ends can no longer name those modules.
- `static` console output routed from inside domain logic.
- Lossy `String::from_utf8_lossy` on incoming message payloads.

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

[Unreleased]: https://github.com/arkCyber/metaText/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/arkCyber/metaText/releases/tag/v0.4.0
