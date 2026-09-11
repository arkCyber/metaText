# metaText

[![CI](https://github.com/arksong/meta-text-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/arksong/meta-text-rust/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![crates.io](https://img.shields.io/badge/crates.io-meta--text-blue)](https://crates.io/crates/meta-text)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](CONTRIBUTING.md)

> metaText — a Web3 decentralized instant messaging client, written in Rust.

`metaText` is the Rust rewrite of the original C `metaText`/toxic prototype. It
starts a local encrypted session, keeps a friend list, and offers both a full
screen terminal interface and a scriptable command line interface.

**Features**

- End-to-end message encryption (ChaCha20-Poly1305) with a self-generated key
- Encrypted peer-to-peer messaging over TCP with a shared passphrase
- Slash-command REPL and a full screen TUI
- Optional durable message history (SQLite)
- TOML configuration with CLI overrides and a persisted session file

## Requirements

- **A stable Rust toolchain** — install with [rustup](https://rustup.rs/). The
  channel and components are pinned in [`rust-toolchain.toml`](rust-toolchain.toml)
  and applied automatically by `rustup`.
- **A C compiler** — required transitively by some crates on Linux.
- No external services are needed to run two peers on the same machine.

## Installation

Install the latest release from crates.io:

```bash
cargo install meta-text
```

Or build it from source:

```bash
git clone https://github.com/arksong/meta-text-rust.git
cd meta-text-rust
cargo build --release --features sqlite,terminal-ui
# binary at target/release/meta-text
```

Prebuilt binaries for Linux, macOS and Windows are attached to every
[release](https://github.com/arksong/meta-text-rust/releases).

## Quick start

```bash
# Default: full screen terminal interface when built with `terminal-ui`,
# otherwise a plain interactive REPL.
cargo run

# Scriptable command line mode
cargo run -- --mode cli

# `run` subcommand: the same interactive REPL, spelled explicitly
cargo run -- run
```

Press `Ctrl+C` (REPL) or `Ctrl+C` / `Ctrl+Q` (TUI) to quit.

The plain REPL greets you with a short welcome banner and then shows a `>> `
prompt each time it is waiting for your next command. The prompt is only drawn
when `stdin` **and** `stdout` are real terminals, so piped or scripted sessions
keep a clean, machine readable stream.

### Useful CLI options

| Option | Description |
| --- | --- |
| `-c, --config <FILE>` | Configuration file (default `config.toml`) |
| `-m, --mode <MODE>` | `tui`, `cli`, `server` or `daemon` (default `tui`) |
| `-l, --log-level <LEVEL>` | `error`, `warn`, `info`, `debug`, `trace` |
| `-D, --debug` | Shorthand for debug logging |
| `--headless` | Run without any user interface |
| `-d, --data-dir <DIR>` | Where the session file lives |
| `-p, --port <PORT>` | Network port for P2P communication |
| `--peer <ADDRESS>` | Peer (`host:port`) to connect to at startup; repeatable or comma-separated; retried until it answers |
| `--passphrase <TEXT>` | Shared secret; peers using the same value derive the same key |
| `--nick <NAME>` | Nickname announced to peers |
| `--bootstrap <ADDRESS>` | Extra peer to dial and keep connected (`host:port`) |
| `--no-encryption` | Disable message encryption (not recommended) |
| `--history-limit <COUNT>` | Number of messages kept in history |

### The `run` subcommand

`meta-text run` starts the interactive REPL and is exactly equivalent to
`--mode cli`. Every option above is *global*, so it may be written either before
or after the subcommand:

```bash
cargo run -- run --port 34567 --nick Alice
# identical to
cargo run -- --mode cli --port 34567 --nick Alice
```

An explicit `run` wins over a conflicting `--mode`, while `--headless` still
suppresses the REPL. Without a subcommand the behaviour is unchanged, so
existing `--mode` invocations keep working.

## Talking to another instance

Start two instances that share a passphrase, and connect one to the other:

```bash
# Terminal 1 — listens on 34567
cargo run -- --mode cli --port 34567 --passphrase "correct horse battery staple" --nick Alice

# Terminal 2 — connects to Alice
cargo run -- --mode cli --port 34568 --passphrase "correct horse battery staple" \
  --nick Bob --peer 127.0.0.1:34567
```

Then, in either terminal:

```
/nick Alice          # announce a name to peers
/peers               # show connected peers
/connect host:port   # dial a peer at any time
/add BOB             # add a conversation partner
/chat 1              # make it active
hello!               # send an encrypted message to every connected peer
```

Sending prints the message id and how many peers the frame was queued for, and
inbound messages appear as `📥 <nickname>: <text>`. When the active
conversation's name matches a connected peer's announced nickname (compared
case-insensitively) the frame goes only to that peer, otherwise it is
broadcast. Receivers acknowledge every message, so the sender also prints
`✅ delivered to <nickname> (#<id>)`.

The transport is TLS-free TCP with an authenticated ChaCha20-Poly1305 payload
per frame; `--passphrase` derives the shared key with Argon2, so instances with
different passphrases cannot read each other's messages (they simply log a
decryption failure).

`/peers` shows the address the listener is bound to plus any address that is
still being retried, which is useful when port `0` was configured (the OS picks
the port) or when the other instance has not started yet.

**Start order does not matter.** Every `--peer` (and `/connect`) address becomes
a *desired peer*: if it is not reachable yet the transport keeps retrying it in
the background and connects as soon as the other instance appears. If the
connection drops later, it is re-established automatically.

**Messages to an offline peer are buffered.** Sending to a contact whose peer is
not connected prints `📨 … buffered as #n` and the transport delivers it
automatically once that peer announces the matching nickname. Each queue holds
up to 32 messages and entries expire after five minutes.

## Interactive commands

| Command | Description |
| --- | --- |
| `/help [command]` | Show the command list, or the help for one command |
| `/readme` | Show the metaText introduction |
| `/info` | Show session and subsystem information |
| `/list` | List your friends (alias `/friends`); the active chat is marked |
| `/peers` | List connected peers (alias `/connections`) |
| `/connect <addr>` | Connect to a peer `host:port` (alias `/join`) |
| `/add <DID> [note]` | Add a friend by DID address |
| `/remove <n\|name>` | Remove a friend (aliases `/rm`, `/del`) |
| `/chat [n\|name]` | Talk to friend `n` or the given name; no argument shows the active chat |
| `/msg <name> <text>` | Send to one peer without switching the active chat (alias `/send`) |
| `/nick [name]` | Change your nickname; no argument shows the current one |
| `/status [text]` | Change your status message; no argument shows the current one |
| `/whoami` | Show your nickname, DID and bound address |
| `/version` | Show the application version and crypto algorithm |
| `/uptime` | Show how long this session has been running |
| `/stats` | Show runtime statistics |
| `/history [n]` | Show the last `n` stored messages (default 20) |
| `/save` | Persist the session (nickname, friends, statistics) to disk |
| `/clear` | Clear the screen (or the TUI scrollback) |
| `/quit` | Leave metaText |

Any other input is sent as a message to the active conversation.

`/msg` addresses a *peer nickname*, like the transport does, so it reaches a peer
that is not in the friend list; an offline peer is buffered as usual. `/remove`
accepts the same 1-based index or name fragment as `/chat`, and clears the active
conversation when that friend was selected.

## Cargo features

All optional subsystems are opt-in so the default build stays light.

| Feature | Enables |
| --- | --- |
| `sqlite` | Durable SQLite persistence for contacts and message history |
| `terminal-ui` | Full screen `crossterm` + `tui` interface for `--mode tui` |
| `postgres`, `mysql` | Alternative SQLx database drivers |
| `tox-protocol` | Pulls in the `tox` crate for the future P2P transport |

```bash
# Persist message history (adds the `/history` command)
cargo run --features sqlite -- --mode cli

# Full screen interface
cargo run --features terminal-ui -- --mode tui
```

Without `sqlite` the `/history` command explains how to enable persistence.
Without `terminal-ui`, `--mode tui` transparently falls back to the plain REPL,
and so does any run whose `stdout` is not a terminal (for example piped input).

## Configuration

`config.toml` (see the file in the repository) groups the settings into
`[app]`, `[network]`, `[database]`, `[crypto]`, `[ui]` and `[logging]`. A
missing configuration file is created with defaults on first start. Command line
flags always win over file values.

The session (nickname, status, friends, statistics) is stored as
`metatext-session.json` inside `--data-dir`; the SQLite database lives at
`database.connection_string`.

## Architecture

| Module | Responsibility |
| --- | --- |
| `app` | Top level coordinator, event loop, slash commands |
| `cli` | `clap` based argument parsing and validation |
| `commands` | Pure parser from input lines to `Command` |
| `config` | TOML configuration load/save/validate |
| `crypto` | Authenticated encryption and Argon2 key derivation |
| `database` | SQLite persistence for contacts and messages |
| `error` | Domain error types and context helpers |
| `network` | Encrypted TCP transport, connection lifecycle and peer registry |
| `tui` | Full screen interface and console output routing |
| `types` | Shared domain types (contacts, events, statistics) |
| `utils` | Small helpers (hex, random, timestamps) |

## Development

```bash
# Everything the CI pipeline runs
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --workspace
cargo test --all-features

# Documentation
cargo doc --no-deps --all-features --open
```

A [`Makefile`](Makefile) wraps the common tasks — run `make` (or `make help`)
to list them:

```bash
make build      # debug build with all features
make test-all   # full test suite
make ci         # fmt + clippy + tests, exactly like CI
```

## Testing

The suite contains unit tests, integration tests that drive the public API, and
end-to-end tests that spawn the compiled binary and script it over stdin.

```bash
cargo test                       # default features
cargo test --all-features        # everything CI runs
cargo test --all-features -- --nocapture   # show test output
```

## Networking model

- **Transport**: plain TCP with a small framing protocol
  (`kind || u32 length || payload`); `kind 1` is the nickname handshake,
  `kind 2` is an encrypted chat message prefixed with a 64-bit message id, and
  `kind 3` acknowledges a received id. Frame lengths are validated before
  allocation.
- **Routing**: a message goes to the peer whose announced nickname matches the
  active conversation, falling back to a broadcast when there is no match.
- **Acknowledgements**: the receiver replies with an ack frame, which the sender
  surfaces as `✅ delivered to <peer> (#<id>)`; delivery is otherwise best
  effort.
- **Key agreement**: a shared `--passphrase` is stretched with Argon2 into the
  session key, so every peer configured with the same passphrase can decrypt
  each other's frames. Without `--passphrase` each process uses a fresh random
  key and messages stay local.
- **Authentication**: every frame is sealed with ChaCha20-Poly1305; a tampered
  or wrong-key frame is rejected and logged instead of being delivered.
- **Connections**: the listener accepts inbound peers up to `--max-connections`,
  and `/peers` reports the live count plus the bound address.
- **Resilience**: a supervisor task retries every desired peer that is not
  connected every couple of seconds, so peers may be started in any order and a
  dropped connection is re-established without user action.
- **Offline queue**: a message addressed to a nickname that is not connected is
  buffered per nickname (32 messages, five minute expiry) and flushed as soon as
  that peer announces itself.

## Known limitations

- The Tox DHT is not wired up: the `bootstrap_nodes` list from `config.toml` is
  shown in `/info` but not dialled, because those entries are Tox public nodes.
  Use `--peer` or `--bootstrap` for addresses reachable over this TCP transport.
- Contact identifiers are treated as opaque DID-like strings; there is no
  per-contact key exchange or friend-request handshake yet.
- Routing matches the active conversation against announced nicknames, so a
  contact is only addressed directly while a peer announcing that name is
  connected. Messages sent meanwhile are buffered, but the queue lives in memory
  only: it is not persisted across restarts.
- Acknowledgements only confirm that a peer received the frame; there is no
  end-to-end message history sync between peers.
- The displayed `identity (DID)` is a random per-session value shown for
  readability; it is not used for cryptography and changes on restart.

## Contributing

Contributions are welcome! Please read [`CONTRIBUTING.md`](CONTRIBUTING.md) for
the development workflow and coding standards, and note that this project is
governed by a [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).

- Use [Conventional Commits](https://www.conventionalcommits.org/) for commit
  messages.
- Run `make ci` (or the equivalent Cargo commands) before opening a pull request.
- Found a security issue? Report it privately per [`SECURITY.md`](SECURITY.md).

## Acknowledgements

metaText began as a Rust rewrite of the original C `metaText`/toxic prototype
and draws on the [Tox](https://tox.chat/) protocol and the `toxic` client for
inspiration. See [`CHANGELOG.md`](CHANGELOG.md) for the release history.

## License

This project is licensed under the **MIT License** — see the [`LICENSE`](LICENSE)
file for details.

Copyright (c) 2024 arkSong &lt;arksong2018@gmail.com&gt;.
