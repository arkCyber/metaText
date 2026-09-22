# metaText

[![CI](https://github.com/arkCyber/metaText/actions/workflows/ci.yml/badge.svg)](https://github.com/arkCyber/metaText/actions/workflows/ci.yml)
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
- Slash-command REPL and a full screen TUI, both driven by the same core service
- Headless core (`--mode core`) that can expose a versioned request/response
  protocol over TCP to out-of-process front-ends
- Offline message queue plus a supervisor that reconnects desired peers
- Optional durable message history (SQLite)
- TOML configuration with CLI overrides, validated before startup, and a
  persisted session file

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
git clone https://github.com/arkCyber/metaText.git
cd metaText
cargo build --release --features sqlite,terminal-ui
# binary at target/release/meta-text
```

Prebuilt binaries for Linux, macOS and Windows are attached to every
[release](https://github.com/arkCyber/metaText/releases).

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

The full screen interface has a small line editor on its input row: `Left` /
`Right` / `Home` / `End` move the cursor, `Delete` and `Backspace` edit, `Ctrl+U`
clears the line, and `Up` / `Down` walk the command history. A draft wider than
the pane scrolls with the cursor and shows a leading `…` for the text that is off
to the left; pasting a paragraph keeps it on one line, because a message line is
what the pane and the transport expect. `Ctrl+L` clears the scrollback and `PgUp`
/ `PgDn` scroll the conversation pane a page at a time; a pane showing older rows
says so in its title, and the friends and peers panes mark a list they cannot
show in full with `· +N more`. (Some terminals reserve `Ctrl+Q` for flow control;
prefer `Ctrl+C` when in doubt.)

The plain REPL greets you with a short welcome banner and then shows a `>> `
prompt each time it is waiting for your next command. The prompt is only drawn
when `stdin` **and** `stdout` are real terminals, so piped or scripted sessions
keep a clean, machine readable stream.

### Useful CLI options

| Option | Description |
| --- | --- |
| `-c, --config <FILE>` | Configuration file (default `config.toml`) |
| `-m, --mode <MODE>` | `tui`, `cli`, `core`, `server` or `daemon` (default `tui`) |
| `-l, --log-level <LEVEL>` | `error`, `warn`, `info`, `debug`, `trace` (overrides `[logging] level`) |
| `-D, --debug` | Shorthand for debug logging |
| `--headless` | Run without any user interface |
| `-d, --data-dir <DIR>` | Where the session file lives |
| `-p, --port <PORT>` | Network port: the TCP listener port, or the UDP port toxcore binds. `0` asks the OS for a free port (read it back from `/whoami` or `/peers`) |
| `--transport <KIND>` | `tcp` (default) or `tox`; selects the transport the core actor drives |
| `--peer <ADDRESS>` | Peer to connect to at startup: `host:port` for tcp, a 76 character Tox address for tox; repeatable or comma-separated |
| `--passphrase <TEXT>` | Shared secret; peers using the same value derive the same key (env `METATEXT_PASSPHRASE`). Not used by tox, which encrypts per friend |
| `--nick <NAME>` | Nickname announced to peers |
| `--bootstrap <ADDRESS>` | Bootstrap node: `host:port` for tcp, `host:port:PUBLIC_KEY` for tox |
| `--ipc-listen <ADDRESS>` | Headless modes only: serve the core protocol on `host:port`. A non-loopback address requires `--ipc-token` |
| `--ipc-token <TOKEN>` | Shared secret a protocol client must present (env `METATEXT_IPC_TOKEN`) |
| `--ipc-rate <COUNT>` | Sustained requests per second one client **address** may use (`0` disables the limit; overrides `[ipc] requests_per_second`) |
| `--ipc-burst <COUNT>` | Burst one client address may spend above the sustained rate (overrides `[ipc] request_burst`) |
| `--no-encryption` | Disable message encryption (not recommended) |
| `--max-connections <COUNT>` | Maximum concurrent connections (default `100`) |
| `--history-limit <COUNT>` | Number of messages `/history` shows by default (`1`-`1000`, default `20`); a `/history <n>` argument still wins |

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

### Peer identity and fingerprints

On top of that shared **session key**, every pair of peers gets key material of its
own, and since this release it is **agreed** rather than derived. Each connection
contributes an ephemeral X25519 key pair (`FRAME_EPHEMERAL`) on top of the persisted
identity both ends announce, and the key a directed message is sealed under is
`HKDF-SHA256(session key, DH(S,S) ‖ DH(E,S) ‖ DH(S,E))`. What that buys, concretely:

- a frame captured from one pair cannot be read or replayed as part of another;
- a passive listener — even one that holds the passphrase and saw both announced
  public keys, which is every other member of the session — **cannot** reproduce the
  key, because one of the inputs needs a static secret that never left either end;
- the key is fresh per connection, so a recorded handshake does not open a later one;
- the passphrase is still required (the session key is part of the derivation), so a
  peer without it gets nothing.

Broadcasts (no active conversation, and anything sent to a peer that announced no
identity) keep using the session key, and so does a message buffered for an offline
peer: there is no connection to agree a key on.

That identity is **persisted**: it is an X25519 key pair kept in
`net-identity.json` in the data directory (the file holds the secret half and is
created owner-only; the public half is what peers see), so it is the same value
after a restart instead of a fresh random string. Two consequences you can use:

- `/whoami` prints a **fingerprint** of it (`A1B2-C3D4-…`, eight groups of four
  hex characters). Read it out on a call and compare it with your peer's — the
  same pair must see the same two fingerprints every run.
- A peer's announced identity is remembered in `net-peers.json`, keyed by the
  nickname it announced. `/peers` lists every pinned identity with its
  fingerprint, and if a peer that was seen before announces a **different**
  identity, it is flagged there (`⚠️ changed since it was pinned`), logged as a
  warning, and counted as `peer_identities_changed` in `/metrics`.

Be clear about the one thing that is *not* covered: the identity is announced, not
signed, so a **first** contact is trust-on-first-use. Somebody who is on the path and
holds the passphrase can present their own identity, agree a key and relay. They
cannot do it silently — the pin records the identity and reports a change, and
comparing fingerprints out of band tells you immediately — but nothing in the
protocol can tell you who owns an identity you have never seen before. Proving that
needs an identity authenticated off-channel, which is what
`docs/ARCHITECTURE.md` §6.2 names as the remaining open item.

`/peers` shows the address the listener is bound to, any address that is still
being retried (useful when port `0` was configured — the OS picks the port — or
when the other instance has not started yet), and the pinned identity of every peer
that has announced one, with its fingerprint.

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
| `/help [command]` | Show the command list, or the help for one command (aliases `/h`, `/?`) |
| `/readme` | Show the metaText introduction |
| `/info` | Show session and subsystem information |
| `/list` | List your friends (alias `/friends`); the active chat is marked |
| `/peers` | List connected peers and the pinned identity of each, with its fingerprint (alias `/connections`) |
| `/connect <addr>` | Connect to a peer `host:port` (alias `/join`) |
| `/add <DID> [note]` | Add a friend by DID address |
| `/requests` | List pending friend requests (Tox only; alias `/pending`) |
| `/accept <n\|key>` | Accept a pending friend request (Tox only) |
| `/reject <n\|key>` | Refuse a pending friend request (Tox only); nothing is sent to the requester (alias `/decline`) |
| `/remove <n\|name>` | Remove a friend (aliases `/rm`, `/del`) |
| `/chat [n\|name]` | Talk to friend `n` or the given name; no argument shows the active chat (alias `/to`) |
| `/msg <name> <text>` | Send to one peer without switching the active chat (alias `/send`) |
| `/me <action>` | Send a third-person action; you and the peer see `* <nick> <action>` (aliases `/action`, `/emote`) |
| `/bin <hexadecimal>` | Send a binary payload (`/bin 00ff10`); the peer sees its size and a preview (aliases `/binary`, `/hex`) |
| `/group [sub]` | Group chats (Tox only): `list`, `invites`, `create <name>`, `rename [@group] <name>`, `join <token>`, `decline <token>` (discard an invitation; alias `reject`), `invite [@group] <peer>`, `send [@group] <text>`, `me [@group] <text>`, `bin [@group] <hex>`, `select <id\|name\|index>`, `leave [@group]` (aliases `/groups`, `/g`) |
| `/nick [name]` | Change your nickname; no argument shows the current one (alias `/nickname`) |
| `/status [text]` | Change your status message; no argument shows the current one |
| `/whoami` | Show your nickname, identity, fingerprint and bound address |
| `/version` | Show the application version and crypto algorithm (alias `/ver`) |
| `/uptime` | Show how long this session has been running |
| `/stats` | Show runtime statistics |
| `/metrics` | Show operational counters as `key=value` lines (queue depth, sheds, subscribers, actor lag) for monitoring |
| `/history [n]` | Show the last `n` stored messages (default: `--history-limit`, shipped `20`) (alias `/log`) |
| `/save` | Persist the session (nickname, friends, statistics) to disk |
| `/clear` | Clear the screen (or the TUI scrollback) (alias `/cls`) |
| `/quit` | Leave metaText (aliases `/exit`, `/q`) |

Any other input is sent as a message to the active conversation. Command names
are matched case-insensitively, and every alias listed above is accepted.

`/msg` addresses a *peer nickname*, like the transport does, so it reaches a peer
that is not in the friend list; an offline peer is buffered as usual. `/remove`
accepts the same 1-based index or name fragment as `/chat`, and clears the active
conversation when that friend was selected.

## Workspace layout

The layering described in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) (§2.1) is enforced by Cargo, not by
review: each layer is its own crate, so a module can only reach what its
`Cargo.toml` declares. There is no way to write an import that skips a layer
without adding the dependency in the open.

```text
meta-text              this package: the `meta-text` binary (composition root)
                       plus an umbrella lib that re-exports every layer
 └─ meta-text-cli      the line oriented REPL front-end
     └─ meta-text-tui  presenter, wording, terminal engine, full screen front-end
         └─ meta-text-core   CoreService / CoreHandle, in-process and TCP clients
             └─ meta-text-backend  crypto, database, network, tox, transport,
                                   config, logging, cli vocabulary
                 └─ meta-text-proto  wire protocol, framing, validation,
                                     shared error/types/helpers
```

| Crate | Reaches | Cannot reach |
| --- | --- | --- |
| [`meta-text-proto`](crates/meta-text-proto/README.md) | nothing in the workspace | every other layer |
| [`meta-text-backend`](crates/meta-text-backend/README.md) | `meta-text-proto` | the service, any front-end |
| [`meta-text-core`](crates/meta-text-core/README.md) | `proto`, `backend` | any front-end; it does **not** re-export `crypto`, `database`, `network` or `transport` |
| [`meta-text-tui`](crates/meta-text-tui/README.md) | `proto`, `core` | `backend` (so: no key, no row, no socket) and `cli` |
| [`meta-text-cli`](crates/meta-text-cli/README.md) | `proto`, `core`, `tui` | `backend` |
| `meta-text` | every layer | — (this is the composition root) |

The umbrella crate keeps the historical `meta_text::…` paths working for
applications and for the integration tests under [`tests/`](tests); an
application that wants the boundary enforced compiles against `meta-text-core`
directly.

Each crate documents itself: the table above links its README, and
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) is the interface contract
(§2.1.1 is the same crate table with the reason for each rule),
[`CHANGELOG.md`](CHANGELOG.md) the release history.

## Cargo features

All optional subsystems are opt-in so the default build stays light.

| Feature | Enables |
| --- | --- |
| `sqlite` | Durable SQLite persistence for contacts and message history |
| `terminal-ui` | Full screen `crossterm` + `tui` interface for `--mode tui` |
| `postgres`, `mysql` | Alternative SQLx database drivers |
| `tox-protocol` | Builds the `tox` module: FFI bindings to the system `libtoxcore` (see [Tox foundation layer](#tox-foundation-layer)) |

### Tox foundation layer

The `tox-protocol` feature compiles `crates/meta-text-backend/src/tox.rs`, a
self-contained transport over
the **system `libtoxcore`** — the same C library the original `metaText` drives
from `metaText.c`. It is deliberately *not* the pure-Rust `tox` crate: that one
is GPL-3.0+, pinned to tokio 0.2, and only exposes low-level DHT/`net_crypto`
primitives with no client object.

`crates/meta-text-backend/build.rs` locates the library and links it (set
`TOXCORE_LIB_DIR` to override,
which is also how the stub build is produced). When the library is missing the
module still compiles, but `ToxClient::start` returns `ToxError::Unavailable`,
so `--all-features` keeps working on machines and CI runners without toxcore.

Install it with `brew install toxcore` (macOS) or
`apt install libtoxcore-dev` (Debian/Ubuntu). Note that `libtoxcore` is
GPL-3.0, so a binary linked against it is a combined work under the GPL-3.0.

```bash
cargo test -p meta-text-backend --features tox-protocol --lib tox::   # unit tests
cargo test -p meta-text-backend --features tox-protocol --lib test_bootstrap_reaches_the_dht -- --ignored
```

```rust
use meta_text::tox::{ToxClient, ToxConfig};

let config = ToxConfig::new("/tmp/metatext", "arkSong");
let client = ToxClient::start(config)?;
println!("my Tox address: {}", client.address()); // 76 hex characters
client.add_friend("D5F0…0831", "hi, please accept")?;
client.shutdown()?;
```

### Talking to another user over Tox

`--transport tox` selects Tox **inside the core actor**: the same `CoreService`
that serves TCP now owns the Tox instance, so contacts, message history,
`/info`, `/history`, the REPL *and* the full screen TUI all work unchanged. Both
ends must build with `--features tox-protocol`:

```bash
# Terminal 1 — /whoami prints the 76 character Tox address to hand out
cargo run --features tox-protocol -- --transport tox --nick Alice --data-dir ~/.metatext-tox

# Terminal 2 — add Alice's address (or pass it as --peer <address>)
cargo run --features tox-protocol -- --transport tox --nick Bob --data-dir ~/.metatext-tox-bob
```

In Alice's session:

```text
/whoami                     # show the 76 character Tox address to hand out
/add <Bob's Tox address>    # send a friend request (alias: /connect)
```

In Bob's session:

```text
/requests                   # list the pending friend requests
/accept 1                   # accept one by index (or /accept <public key>)
/reject 2                   # refuse one instead; nothing is sent to the requester
/peers                      # friends, their nickname and connection state
/chat 1                     # make a friend the active conversation
hello Alice                 # send (or /msg <name> <text>)
/me waves                   # both sides see: * Bob waves
/bin 00ff10                 # both sides see: [binary, 3 B] 00ff10
/group create Team          # create a group chat
/group invite @Team Alice   # invite a friend into it
/group send @Team hi all    # both sides see: 👥 [Team] Bob: hi all
```

`--bootstrap host:port:PUBLIC_KEY` points the instance at a private DHT node; the
public Tox node list is built in and is used when the flag is omitted. `--port`
pins the UDP port toxcore binds (unset means "let the OS choose").

An invitation can be answered *no* as well as *yes*: `/group invites` prints one
token per line, `/group join <token>` accepts one and `/group decline <token>`
(alias `/group reject`) discards it. Declining is local — toxcore has no
"decline" message, so nothing is sent to the inviter, exactly like `/reject` on a
friend request — and it is not a block: the same peer can invite again.

Both halves of the Tox queue are **durable**: a friend request that has not been
answered and a payload buffered for an offline friend are written to
`tox-state.json` in the data directory (next to `tox-savedata.bin`) and read back
at startup, so a request can still be answered — and a message the sender was told
was waiting still arrives — after a restart. See
[Configuration](#configuration) for the files a data directory holds, and
`docs/ARCHITECTURE.md` §3.9 for the format.

Tox already encrypts peer-to-peer between friends, so the metaText envelope is
*not* applied on top of it: `CoreTransport::provides_encryption` reports that and
the core sends the plaintext body. A TCP session still wraps every payload with
ChaCha20-Poly1305.

The full flow is asserted by an opt-in end-to-end test (it needs outbound UDP to
reach the public Tox DHT):

```bash
cargo test -p meta-text --features tox-protocol,terminal-ui --test tox_core_transport_test -- --ignored --nocapture
# test_two_cores_exchange_a_message_over_tox: friend request → /accept → connect → message
```

When the result matters, run one of these tests at a time. A single batch can miss
the 120 s friend-request budget even though the tests serialise themselves, because
the public DHT rate-limits a host that bootstraps repeatedly; the failure moves
between the tests from run to run and disappears when the named test is run on its
own. `docs/ARCHITECTURE.md` §7 says the same thing next to the command.

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
`[app]`, `[network]`, `[database]`, `[crypto]`, `[ui]`, `[logging]` and `[ipc]`.
A missing configuration file is created with defaults on first start. Command line
flags always win over file values.

The file is **validated before anything is opened**: every problem is reported
at once and the process refuses to start on an unusable file. Documented limits
include the port range, both connection and message sizes, the accepted database
backends and crypto algorithm/KDF names, and the log level and file settings.
`config.network.connection_timeout` drives outbound dial timeouts,
`config.network.keepalive_interval` the liveness probe cadence — shortening it keeps a
NAT mapping alive, while a slower probe is refused because it would be dropped for
silence (see [Known limitations](#known-limitations)) — and the
`[logging]` section selects the level, destination, whether a console layer
exists, and how the log file is rotated: a file is rolled over when it reaches
`rotation_size_mb` **and** when the day changes, so a busy day produces
`meta-text.log.2026-09-14`, `meta-text.log.2026-09-14.001`, … and `max_files`
bounds how many of them are kept (the active file included, oldest removed
first).

The `[ui]` section is what the full screen interface honours: `theme` (`dark` or
`light`, and another name is refused), `enable_colors` (with `false` every style is
the terminal's own), `enable_mouse` (the wheel scrolls the conversation pane, at the
cost of the terminal's own text selection unless a modifier is held) and
`auto_scroll` (with `false` the pane keeps showing the rows it shows while output
arrives, and says so in its title). The presentation layer is written against the
core protocol rather than the configuration, so the composition root translates this
section into the interface's options — and `message_format` is **not implemented**:
message lines are rendered by that layer, which wraps the body in the direction, the
peer and the delivery report. Any value other than the shipped one is refused at
startup instead of being accepted and ignored, the same way
`network.enable_upnp = true` is.

The `[database]` and `[crypto]` sections are held to the same rule, so a key an
operator can see is a key the binary keeps. `enable_migrations` says who owns the
schema: with `true` it is created (and upgraded in place) when the store is opened,
with `false` the store is checked at startup and refused unless both tables and every
column this build writes to are already there — a database that was never initialised
used to open, report itself persistent and fail one write at a time. Of the `[crypto]`
keys, `enable_encryption`, `algorithm` and `kdf` are what the cipher uses;
`key_rotation_days` is **not implemented and not implementable locally** (the session
key is the one every peer with the passphrase derives, and the identity key is what
peers pin, so a timer here would leave them unable to read what this instance writes),
so `0` is the only accepted value and anything else is refused; `enable_pfs` has no
downgrade path to select — the per-connection agreement is part of the identity
handshake — so `false` is refused, and `true` describes what the TCP transport does
(the Tox transport carries no identity or ephemeral frames and leaves payload
encryption to toxcore).

The session (nickname, status, friends, statistics) is stored as
`metatext-session.json` inside `--data-dir`; the SQLite database lives at
`database.connection_string`. Two files hold the network identity: `net-identity.json`
(the instance's own X25519 key pair — the secret half is in there, so it is created
owner-only — and the value peers pin) and `net-peers.json` (what each peer
announced, keyed by nickname, bounded and sorted; see
[Peer identity](#peer-identity-and-fingerprints)). The Tox transport keeps its own
two files in the same directory: `tox-savedata.bin` (the identity and friend list,
written by `toxcore`) and `tox-state.json` (the unanswered friend requests and the
outbox, written by the transport; see
[Talking to another user over Tox](#talking-to-another-user-over-tox)).

User input is validated too, so the same rules apply to the CLI, the TUI and a
socket client: nicknames are 1–64 characters, statuses up to 256, contact
identifiers up to 128 and single tokens, and messages must be non-empty, within
`app.max_message_length` characters, and free of control characters other than
newlines and tabs. A rejected request changes nothing.

**What a peer sends is checked the same way**, which is the direction that cannot
be trusted: a message body must be valid UTF-8 and free of control characters
(the rule the send path already applied), and every label a peer chooses — the
nickname in a TCP greeting, a Tox friend's announced name, a conference title, a
participant's name — must be 1–64 characters with no control character. A refused
body is reported as `message_undecodable` (with the reason, and the group when it
came from one), counted and not stored; a refused label leaves the peer *unnamed*
while its connection, frames and counters stay, so a peer is identified by address
or short key instead of by a string that would clear the reader's screen. Nothing
is repaired or filtered: showing a body with the escape sequences stripped would
display something the peer did not send, which this project refuses to do. TCP
greetings refused for this reason are counted in `/metrics` as
`greetings_refused`.

### Logging

```toml
[logging]
level = 'info'                 # error | warn | info | debug | trace
file_path = 'logs/meta-text.log'
enable_console = true
enable_file = true
rotation_size_mb = 10          # roll a file over when it reaches this size
max_files = 5                  # files to keep, the active one included
```

`RUST_LOG` overrides `level` when it is set. Files are rotated **daily and by
size**: `{file_path}.YYYY-MM-DD` is the day's first file, and it continues as
`{file_path}.YYYY-MM-DD.001`, `.002`, … once it reaches `rotation_size_mb`.
`max_files` bounds the total — the active file counts towards it and the oldest
files are removed first. A write is never split across two files, a restart
counts the bytes already in today's file, a missing log directory is created, and
files this appender did not write are never deleted.

## Architecture

`metaText` separates the **user interface** from the **core application**. The
core owns every piece of domain state (config, crypto, storage, transport,
contacts) and runs headless; the CLI and TUI are pure presentation clients that
reach it only through a versioned request/response protocol.

```text
   meta-text-cli (REPL)   meta-text-tui (full screen + presenter)
              \                        /
               \   ipc protocol (ICD) /
          +--------------------------------------+
          | ipc::client  /  ipc::server          |   meta-text-core
          +--------------------------------------+
                          |
          +--------------------------------------+
          | ipc::core  CoreService + CoreHandle  |   meta-text-core (actor)
          +--------------------------------------+
             |            |            |
          crypto       database     transport (TCP or Tox)
                                       /          \
                                  network         tox   meta-text-backend
```

The rules that keep the halves independent:

- **Presentation never touches a subsystem.** `meta-text-tui` and
  `meta-text-cli` may only use `CoreClient` from `meta-text-core`; they cannot
  name `crypto`, `database`, `network` or `transport`, because that crate does
  not re-export them.
- **The core never formats text.** Replies carry typed data; every string the
  user sees lives in `meta-text-tui`'s `presenter` / `text`, so wording can
  change without touching the core.
- **One vocabulary.** Both the in-process handle and the TCP transport encode
  the same `Request` / `Reply` / `CoreEvent` values, so a front-end can move out
  of process without changing a line of presentation code.

| Crate | Module | Responsibility |
| --- | --- | --- |
| `meta-text-core` | `ipc` | Versioned interface between the user interfaces and the core |
| `meta-text-proto` | `ipc::protocol` | `Request` / `Reply` / `CoreEvent` / `ErrorInfo` wire types |
| `meta-text-proto` | `ipc::framing` | Bounded length-prefixed framing for stream transports |
| `meta-text-proto` | `ipc::validation` | Boundary validation of every front-end supplied value |
| `meta-text-core` | `ipc::core` | `CoreService` (backend actor) and `CoreHandle` (client handle) |
| `meta-text-core` | `ipc::client` | `CoreClient` trait, `LocalClient`, `RemoteClient` |
| `meta-text-core` | `ipc::server` | TCP endpoint that exposes the core to other front-ends |
| `meta-text-tui` | `presenter`, `text` | Command dispatch and all user-visible wording |
| `meta-text-tui` | `ui::tui` | Full screen front-end |
| `meta-text-cli` | `ui` | Line oriented REPL front-end |
| `meta-text-tui` | `tui` | Terminal rendering engine (widgets, scrollback sink) |
| `meta-text-backend` | `cli` | `clap` based argument parsing and validation |
| `meta-text-tui` | `commands` | Pure parser from input lines to `Command` |
| `meta-text-backend` | `config` | TOML configuration load/save/validate |
| `meta-text-backend` | `crypto` | Authenticated encryption and Argon2 key derivation |
| `meta-text-backend` | `database` | SQLite persistence for contacts and messages |
| `meta-text-proto` | `error` | Domain error types and context helpers |
| `meta-text-backend` | `network` | Encrypted TCP transport, connection lifecycle and peer registry |
| `meta-text-backend` | `identity`, `trust` | The announced X25519 identity and the pins that catch a change |
| `meta-text-backend` | `transport` | The abstraction the actor drives (TCP or Tox) |
| `meta-text-proto` | `types`, `utils` | Shared domain types (contacts, events, statistics) and helpers |

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the interface contract,
the aerospace-inspired design controls this refactor applies, and the audit
findings that remain open.

### Modes

| Mode | Behaviour |
| --- | --- |
| `--mode cli` / `run` | Interactive REPL front-end |
| `--mode tui` | Full screen front-end (falls back to the REPL without a TTY) |
| `--mode core` | Headless backend only |
| `--mode server` / `daemon` | Headless backend, typically as a service |
| `--headless` | Starts the backend without attaching any interface |

Any headless mode can expose the protocol to other front-ends:

```bash
# Host the backend and accept protocol clients on loopback
meta-text --mode core --ipc-listen 127.0.0.1:45999 --ipc-token "$METATEXT_IPC_TOKEN"
```

A client speaks JSON frames with a 4-byte big-endian length prefix; the same
`ClientMessage` / `ServerMessage` types are used in-process and on the wire, so
`ipc::client::RemoteClient` (Rust) and any other language can drive the backend:

```text
-> [len] {"type":"hello","protocol_version":9,"token":"…","client":"my-ui"}
<- [len] {"type":"welcome","protocol_version":9,"session":{…}}
-> [len] {"type":"request","id":1,"request":{"type":"ping","echo":"hi"}}
<- [len] {"type":"response","id":1,"result":{"status":"ok","reply":{"type":"pong","echo":"hi"}}}
<- [len] {"type":"response","id":2,"result":{"status":"err","error":{"code":"unauthorized","severity":"recoverable","message":"…"}}}
<- [len] {"type":"event","event":{"type":"message_received","peer":"Bob",…}}
```

A response is tagged `status: "ok"` (payload under `reply`) or `status: "err"`
(`ErrorInfo` under `error` — the tag is `err`, not `error`), and a refusal leaves the
session usable: the same connection serves the next request.

`ipc::server` refuses remote `Shutdown`, validates the protocol version, requires
the token when one is configured, and bounds the handshake timeout, the number of
attached clients and each client's output queue. It also rate limits *by client
address*: the allowance lives in `[ipc]` (`requests_per_second`, `request_burst`),
`--ipc-rate` / `--ipc-burst` override it for one run, and a budget belongs to the
address rather than the connection — so a client that was shed cannot reset it by
reconnecting, while an address nobody has heard from for ten minutes is forgotten.
A shed request is answered with a `backpressure` error and a client that keeps
flooding is detached.

## Examples

[`examples/`](examples) holds one runnable program per application case. They are
compiled by `cargo test --workspace` and linted by `cargo clippy --all-targets`, so
they stay in step with the public API, and they are hermetic: a temporary data
directory, loopback addresses, nothing written into the working tree.

| Example | Application case | Command |
| --- | --- | --- |
| `embed_core` | Embed the headless core: requests, refusals, events, clean shutdown | `cargo run --example embed_core` |
| `repl_session` | Attach the REPL to an embedded core (scriptable over a pipe) | `printf '/info\n/quit\n' \| cargo run --example repl_session` |
| `tui_session` | Attach the full screen interface | `cargo run --features terminal-ui --example tui_session` |
| `peer_chat` | Two instances exchange an encrypted message over TCP | `cargo run --example peer_chat` |
| `remote_frontend` | A front-end in another process, over the IPC socket | `cargo run --example remote_frontend` (with a `--mode core --ipc-listen 127.0.0.1:45999` instance running) |
| `pin_identity` | The announced identity, the pair secret and the pin table | `cargo run --example pin_identity` |
| `tox_transport` | The Tox transport directly (needs `libtoxcore`) | `cargo run --features tox-protocol --example tox_transport` |

`embed_core` also notes the spelling an application uses when it depends on
`meta-text-core` directly instead of on the umbrella crate.

## Development

```bash
# Everything the CI pipeline runs
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --workspace -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --workspace

# Documentation, browsable
cargo doc --no-deps --all-features --workspace --open
```

A [`Makefile`](Makefile) wraps the common tasks — run `make` (or `make help`)
to list them:

```bash
make build      # debug build with all features
make test-all   # full test suite
make examples   # build every runnable example
make ci         # fmt + clippy + tests + docs, exactly like CI
```

## Testing

The suite contains unit tests, integration tests that drive the public API, and
end-to-end tests that spawn the compiled binary and script it over stdin.

```bash
cargo test --workspace                  # default features, every crate
cargo test --workspace --all-features   # everything CI runs
cargo test --workspace --all-features -- --nocapture   # show test output
```

`--workspace` matters: the unit tests of the five member crates are only run for
the package they belong to, so a bare `cargo test` skips four of them.

The full screen interface is covered by a second suite, next to the crate tests
because it needs a terminal: [`scripts/tui-e2e`](scripts/tui-e2e) drives real
processes on a pseudo terminal and asserts on the screen, not on the raw output.

```bash
make tui-e2e     # build with `terminal-ui`, then run both phases
```

## Networking model

- **Transport**: plain TCP with a small framing protocol
  (`kind || u32 length || payload`); `kind 1` is the nickname handshake,
  `kind 2` is an encrypted chat message prefixed with a 64-bit message id,
  `kind 3` acknowledges a received id, and `kind 8` is a keepalive (empty, see
  below). Frame lengths are validated before allocation.
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
  and `/peers` reports the live count plus the bound address. That count is
  *reclaimed*: a peer has `[network] connection_timeout` (the same value that
  bounds a dial, 30 s by default) to announce its nickname, and a socket that
  connects and says nothing is closed when the deadline expires instead of holding
  a slot forever.
- **Bounded per-peer queue**: each connection has a queue of at most 256 frames for
  its writer task. A peer that stops reading (frozen, wedged behind a NAT, or
  hostile) fills it, and further frames are dropped and counted — `/metrics` shows
  the count as `payloads_dropped` — rather than growing memory at the sender. A
  dropped frame is lost, not buffered: the message is still reported undelivered to
  the caller.
- **Resilience**: a supervisor task retries every desired peer that is not
  connected every couple of seconds, so peers may be started in any order and a
  dropped connection is re-established without user action.
- **Liveness**: each connection sends an empty keepalive frame (`kind 8`) to every
  greeted peer every `[network] keepalive_interval` (20 s by default; see the
  [known limitations](#known-limitations) for the one direction it may be tuned in), and
  a connection that has received nothing for 60 s is
  closed with a `PeerDisconnected` event that says so. This is what detects a peer
  that vanished without a FIN — a machine that lost power, or a NAT that dropped its
  mapping — which a plain read cannot distinguish from an idle peer. The deadline
  arms only after the peer has *sent* a keepalive, so a build from before that frame
  kind (which ignores it and never sends one) keeps the old behaviour instead of
  being dropped for being quiet.
- **Offline queue**: a message addressed to a nickname that is not connected is
  buffered per nickname (32 messages, five minute expiry) and flushed as soon as
  that peer announces itself.

## Known limitations

- The Tox DHT is dialled by the Tox transport. `config.network.bootstrap_nodes`
  still holds `host:port` entries for the **TCP** transport and is not used by
  Tox; the Tox transport uses the public list compiled into
  `tox::default_bootstrap_nodes` plus one optional `--bootstrap host:port:KEY`.
- On Tox, a payload for a known-but-offline friend is buffered per friend
  (32 payloads, 5 minute expiry) and flushed when the friend connects, exactly
  like the TCP outbox.
- On Tox the buffered payloads and the unanswered friend requests **survive a
  restart**: they are written to `tox-state.json` next to `tox-savedata.bin` in
  the data directory, keyed by public key, and read back at startup. The TCP
  transport's outbox is still memory only, and the queue file is bounded by the
  same caps the transport advertises (32 payloads per friend, 128 requests), so it
  cannot grow on a remote party's schedule.
- Socket clients are rate limited per client **address** (256 req/s with a 512
  burst, configurable in `[ipc]`), so a client cannot refill its allowance by
  reconnecting; a client that keeps flooding is detached. A zero rate disables
  the limit.
- Contact identities are persisted X25519 public keys (`net-identity.json`), the
  identity a peer announced is pinned (`net-peers.json`), and pair keys are agreed
  per connection (see [Peer identity and fingerprints](#peer-identity-and-fingerprints)).
  What remains is that the identity is **announced, not signed**: a first contact is
  trust-on-first-use, so an active participant that holds the passphrase can present
  its own identity and relay until a fingerprint comparison catches it
  (`docs/ARCHITECTURE.md` §6.2).
- On TCP, routing matches the active conversation against announced nicknames, so
  a contact is only addressed directly while a peer announcing that name is
  connected. Messages sent meanwhile are buffered per nickname, but that queue
  lives in memory only: it is not persisted across restarts (unlike the Tox queue,
  which is keyed by public key — see the bullet above). The pin file would give a
  durable queue a stable key to name the peer by, but such a queue would hold
  payloads sealed with the *session* key (there is no connection to agree a key on
  while the peer is away), so it waits for the sealing step to move to delivery
  time (`docs/ARCHITECTURE.md` §6.17).
- Acknowledgements only confirm that a peer received the frame; there is no
  end-to-end message history sync between peers.
- A peer must announce itself within `[network] connection_timeout` (see
  [Networking model](#networking-model)), which closes the *pre-authentication*
  hole: a silent socket cannot hold a connection slot; an established connection is
  probed by the keepalive of the same section. Two limits remain. The idle deadline
  (60 s, three default cadences) is deliberately **not** configurable: it bounds the
  peer's silence, and the peer may be a default one, so it is what keeps two ends
  comparable. The cadence *is* configurable, but in one direction only —
  `[network] keepalive_interval` accepts 1–20 s, which is exactly the tuning a
  deployment behind a NAT that expires an idle mapping sooner than 20 s needs, while a
  *slower* probe is refused at startup because a peer enforcing the default deadline
  would drop it (`docs/ARCHITECTURE.md` §6.20). And half-open
  detection needs *both* ends to send keepalives: against a peer built before that
  frame kind the connection is never dropped for silence
  (`docs/ARCHITECTURE.md` §6.20).
- `/whoami` prints a *stable* identity and a fingerprint to compare with a peer
  (`/peers` prints a peer's). Comparing them is what catches a substituted identity;
  the fingerprints themselves are not a second identity and are not a signature.
- Stored history is bounded on **display**, not by retention: `--history-limit`
  (1–1000, default 20) is the page `/history` shows when it is given no argument, and
  nothing deletes older records, so the store grows with the conversation. Key rotation
  is not implemented either — and not implementable locally, since the session key is
  shared with every peer that has the passphrase and the identity key is what peers pin
  — so `[crypto] key_rotation_days` accepts only `0` and refuses anything else instead of
  pretending to rotate (`docs/ARCHITECTURE.md` §5, A46 and A47).
- `/whoami`'s group count is the transport's own list, taken when the command runs, so
  under Tox it includes a conference toxcore rejoined after a restart without this
  session asking for it. A transport without groups (TCP) has none to report, and the
  line says so instead of printing a number.

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
