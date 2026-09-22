# metaText architecture and interface control document

This document is the design authority for the split between the metaText user
interfaces (CLI, TUI) and the core application service. It records the interface
contract, the design controls applied, the audit that motivated the refactor and
the findings that are still open.

Version: 0.4.0 — protocol version 9 (serves 3..=9).

Protocol history:

| Version | Change |
| --- | --- |
| 1 | Initial request/reply/event vocabulary. |
| 2 | Added `CoreEvent::MessageUndecodable`; undecodable payloads are reported instead of being replaced with U+FFFD. |
| 3 | Added the transport identity (`transport`, `public_identity`, `pending_requests`, `supports_friend_requests`) to `SessionInfo` and the friend-request vocabulary: `Request::PeerRequests`, `Request::AcceptPeerRequest`, `Reply::PeerRequests`, `Reply::PeerRequestAccepted`, `CoreEvent::PeerRequestReceived`. |
| 4 | Added `MessageKind` (`/me` actions): `Request::SendMessage` and `CoreEvent::MessageReceived` carry a `kind` that defaults to `text`, so a version 3 client keeps working. Also added `Request::Metrics` / `Reply::Metrics` (`MetricsView`) for operational monitoring, and `ContentType` (`Text` / `Binary`) so the protocol stays text-only (a binary body travels as hexadecimal) while the transports carry real bytes. |
| 5 | Added group chat: `Request::{Groups, CreateGroup, JoinGroup, InviteToGroup, SendGroupMessage, LeaveGroup}` with matching replies, `CoreEvent::{GroupMessageReceived, GroupChanged, GroupInviteReceived}`, and the `supports_groups` / `groups_supported` capabilities on `SessionInfo` / `MetricsView`. |
| 6 | Added the pending-invitation list (`Request::GroupInvites` / `Reply::GroupInvites`), so a front-end that attaches after `GroupInviteReceived` (a broadcast event, only seen live) can still read the token and join; `Request::RenameGroup` / `Reply::GroupRenamed`, because a group's name is conference-wide state shared with every participant, not a local label; `Request::RejectPeerRequest` / `Reply::PeerRequestRejected`, so a pending friend request can be answered *no* as well as yes; and two `MetricsView` counters (`friend_requests_dropped`, `group_invites_dropped`) that make the bounded request/invitation lists observable. |
| 7 | Added `Request::DeclineGroupInvite` / `Reply::GroupInviteDeclined` (A40): the invitation-side twin of `RejectPeerRequest`, so a bounded list of invitations can be answered *no* without joining a conference and leaving it again. |
| 8 | Added the read-only half of the identity work (§3.10): `SessionInfo::identity_fingerprint` (a fingerprint a user compares out of band), `SessionInfo::peer_identities` (`PeerIdentityView`: what each peer announced, with a per-peer `changed` flag), and two `MetricsView` counters (`peer_identities_changed`, `peer_identities_refused`). Every field is `#[serde(default)]`, so a version 3 front-end decodes the snapshot unchanged. |
| 9 | `CoreEvent::MessageUndecodable` names the *reason* a payload cannot be shown and the group it arrived in, because the rule it reports was widened from one check to two (not valid UTF-8, and no control character — the rule the send path always applied, now applied to what arrives), and `MetricsView::greetings_refused` counts TCP greetings whose nickname cannot be displayed. The event was *extended* rather than joined by a new tag, because an unknown tag is a decode error for every front-end built before it while an unknown field is ignored; all new fields are `#[serde(default)]`. |

`MIN_SUPPORTED_PROTOCOL_VERSION` (3) and `PROTOCOL_VERSION` (9) define the window
the endpoint serves; anything outside is rejected with `unsupported_protocol`, and
the rejection names the window. A version 3 front-end therefore keeps working
against a version 9 core, which is what makes the two independently upgradable.

A version literal is part of the contract, so a test asserts the constant *by
hand* (`ipc_socket_test::test_protocol_version_is_announced`). A test that echoes a
constant back to itself cannot notice a missing bump; the literal has to be
updated in the same change that changes the wire format. The documents are held to
the same rule rather than to a proofreader: `doc_contract_test` compares every
version literal and window claim in this file and in `README.md`, plus the wire
bounds both documents quote, against the constants (A52). It reads the five member
crates' READMEs the same way, because each of those is a crates.io landing page and
`meta-text-proto`'s quotes the protocol window as well.

## 1. Scope

`metaText` must run the same backend behind several presentation front-ends. The
CLI and the full screen TUI are only the first two; an embedding host (for
example a desktop/mobile shell) must be able to attach without re-implementing
any messaging logic and without linking the encryption, storage or transport
code directly.

Requirements derived from that goal:

| ID | Requirement |
| --- | --- |
| R1 | The backend must run without any user interface attached. |
| R2 | Every front-end must drive the backend through one documented interface. |
| R3 | A front-end must not be able to corrupt or bypass domain invariants. |
| R4 | The interface must be usable in-process and out-of-process. |
| R5 | Failures must be typed, bounded and non-fatal wherever possible. |
| R6 | The backend must remain serviceable after a rejected request. |
| R7 | Resource use must be bounded: queues, frames, connections, handshakes. |
| R8 | Behaviour must be reproducible under test, including end-to-end. |

## 2. Architecture

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
          crypto       database    transport (TCP or Tox)
                                       /          \
                                  network         tox   meta-text-backend
```

### 2.1 Layers and allowed dependencies

| Layer | Modules (and the crate that owns them) | May depend on |
| --- | --- | --- |
| Presentation | `presenter`, `text`, `commands`, `tui`, `ui::tui` (`meta-text-tui`); `ui` (`meta-text-cli`) | `ipc::client`, `error` |
| Interface | `ipc::protocol`, `ipc::framing`, `ipc::validation` (`meta-text-proto`); `ipc::client`, `ipc::server` (`meta-text-core`) | `error`, `utils` |
| Transport | `transport` | `network`, `tox`, `tox_store`, `crypto`, `identity`, `trust`, `config`, `cli`, `types` |
| Backend | `ipc::core` (`meta-text-core`) | `config`, `crypto`, `database`, `transport`, `types`, `cli` |
| Subsystems (`meta-text-backend`) | `crypto`, `database`, `network`, `config`, `tox`, `tox_store`, `identity`, `trust` | `error`, `types`, `utils` |

Dependency direction is strictly downward, and since §2.1.1 each layer is also a
crate, so the direction is checked by the compiler rather than by review. In
particular:

- The presentation layer never names `crypto`, `database`, `network` or `transport`.
  This is the mechanical expression of R3: a UI cannot construct a key, write a
  row or open a socket. It also means no front-end knows which transport is in
  use: `--transport tcp|tox` is resolved inside `CoreService`.
- The backend never formats user-visible text; every string lives in
  `presenter` / `text` (crate `meta-text-tui`), so wording can change without
  touching the core and the core stays testable without a terminal.
- `tui` is a rendering engine only. It reports `TuiInput::Line` / `TuiInput::Quit`
  and renders `TuiInfo` plus the output sink; it has no knowledge of `AppEvent`.

### 2.1.1 Enforced by Cargo

The table above is not a convention: every layer is a crate of its own, so an
import that skips a layer does not compile. The workspace root is the `meta-text`
package (the binary and an umbrella lib); `crates/` holds five member crates:

| Crate | Contents | Depends on |
| --- | --- | --- |
| `meta-text-proto` | `error`, `types`, `utils`, `ipc::{protocol, framing, validation}` | nothing in the workspace |
| `meta-text-backend` | `crypto`, `database`, `network`, `tox`, `tox_store`, `identity`, `trust`, `transport`, `config`, `logging`, `cli` | `proto` |
| `meta-text-core` | `ipc::{core, client, server}` | `proto`, `backend` |
| `meta-text-tui` | `presenter`, `text`, `commands`, `tui` (rendering engine), `ui::tui` | `proto`, `core` |
| `meta-text-cli` | `ui` (the line oriented REPL) | `proto`, `core`, `tui` |
| `meta-text` | `src/main.rs` (composition root) and the umbrella lib | every layer |

What that makes mechanical rather than reviewable:

- **A front-end cannot name a subsystem.** `meta-text-core` re-exports the
  configuration and command line vocabulary but *not* `crypto`, `database`,
  `network`, `transport` or `tox`: those are `pub(crate)` aliases inside it, so
  `meta-text-tui` and `meta-text-cli` have no path to them at all. R3 is a
  compile error, not a rule.
- **A subsystem cannot name the service.** `meta-text-backend` sits one crate
  below `CoreService`, and two below a front-end.
- **The wire contract is a leaf.** `meta-text-proto` cannot reach anything, so
  framing, validation and the request vocabulary cannot silently grow a
  dependency on a transport or a database.
- **The presentation split is a direction.** The REPL depends on the TUI crate,
  never the other way round, because the presenter and the console router are
  shared by both front-ends and live in `meta-text-tui`.
- **The umbrella is the composition root.** `meta-text` re-exports every layer so
  the binary and `tests/` keep the historical `meta_text::…` paths; the rules
  above apply to the five member crates, which is where new code goes.

The default build stays light: `sqlite` / `postgres` / `mysql` are declared by
the crate that links SQLx (`meta-text-backend`, and `meta-text-proto` for the
error conversion), `tox-protocol` by the crate that binds `libtoxcore`
(`meta-text-backend`, where `build.rs` lives), and `terminal-ui` by the crate
that links `crossterm`/`tui` (`meta-text-tui`). The root package forwards each
one, so `cargo build --features sqlite,terminal-ui` reads exactly as before.

### 2.1.2 The transport abstraction

`CoreService` owns a `CoreTransport`, not a concrete transport:

```rust
pub enum CoreTransport {
    Tcp(NetworkManager),
    #[cfg(feature = "tox-protocol")]
    Tox(Box<ToxTransport>),
}
```

The surface is deliberately small: lifecycle (`start`/`shutdown`), identity
(`set_nickname`, `set_status`, `set_identity`, `local_address`, `public_identity`),
peer management (`request_peer`, `connect_all`, `desired_peers`), delivery
(`send_payload`) and the friend-request vocabulary (`pending_requests`,
`accept_request`, `supports_friend_requests`). An enum is used instead of a trait
object because the two transports have no common async interface without an
`async_trait`-style shim, and the enum keeps the actor allocation-free and `Send`.

Three properties make the two transports interchangeable to the actor:

1. **One event inbox.** Both report `AppEvent`s into the *same* bounded channel
   the actor already drains. `ToxTransport` runs a `spawn_blocking` bridge task
   that owns the (single-consumer) `ToxClient`, translates callbacks and pushes
   events; the bridge stops as soon as the inbox closes, so shutdown cannot
   deadlock against a full queue.
2. **One confidentiality contract.** `CoreTransport::provides_encryption` tells
   the core whether the transport already encrypts. TCP returns `false`, so the
   core wraps the payload with `CryptoManager`; Tox returns `true`, so the
   plaintext body is sent and toxcore's peer-to-peer encryption is used. Either
   way the actor only sees a decrypted payload on receive.
3. **No blocking on the actor.** Every `ToxSender` call blocks until the toxcore
   worker answers, so all of them are issued through `spawn_blocking` (or from
   the bridge task). Nothing in `CoreService` waits on toxcore inline.

### 2.2 Runtime shape

`CoreService` is an **actor**. It owns configuration, crypto, storage, transport,
contacts and statistics on a single task, so:

- no lock guards domain state and no front-end can observe a half-applied update;
- requests are serialised, which makes the observable order of mutations
  deterministic (R8);
- front-ends only hold a `CoreHandle`: a bounded command queue plus a broadcast
  event stream.

Lifecycle: `CoreService::new` (no I/O) → `start` (bind sockets, open storage,
announce nickname) → `spawn` (detach onto a task) → requests/events →
`Request::Shutdown` (persist, close transport, close storage).

## 3. Interface control document

### 3.1 Framing

```text
+----------------+------------------------------+
| u32 big-endian | payload (UTF-8 JSON)         |
| length (>= 1)  | exactly `length` bytes       |
+----------------+------------------------------+
```

- `MAX_FRAME_LEN` (1 MiB) is validated **before** any buffer is allocated, so an
  attacker-controlled length cannot cause an allocation storm.
- A zero length prefix is rejected; a truncated frame is reported
  (`UnexpectedEof`), never silently accepted.
- A clean end of stream is `Ok(None)` and is distinguishable from a truncated
  frame by reading the first header byte separately.

### 3.2 Session

Every connection starts with `ClientMessage::Hello`:

```json
{"type":"hello","protocol_version":9,"token":"…","client":"tui/0.4.0"}
```

The server answers `Welcome` (carrying a `SessionInfo` snapshot and its *own*
`protocol_version`) or `Rejected` with a typed error, then closes. A client may open
with any version in the window (`MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION`);
the example above sends the current one, and the server answers with its own, so the
two sides can tell which version the session is running at. The handshake is bounded
by a timeout, and a client that does not greet in time is dropped without affecting
other clients.

Rejection causes:

| Cause | `ErrorCode` |
| --- | --- |
| First frame is not `Hello` | `invalid_request` |
| `protocol_version` outside `MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION` | `unsupported_protocol` |
| Token missing or wrong | `unauthorized` |

The token comparison folds the length difference into the result instead of
returning early, so it does not leak the secret through timing.

### 3.3 Requests

Internally tagged (`{"type": …, …}`) so an unknown kind fails fast instead of
being misread. One variant per user-visible operation:

`ping`, `session_info`, `statistics`, `list_contacts`, `add_contact`,
`remove_contact`, `set_nickname`, `set_status`, `select_conversation`, `connect`,
`send_message`, `history`, `save_session`, `peer_requests`, `accept_peer_request`,
`reject_peer_request`, `groups`, `group_invites`, `create_group`, `join_group`,
`rename_group`, `invite_to_group`, `send_group_message`, `leave_group`, `metrics`,
`shutdown`.

`rename_group` changes state that is not ours alone: the transport forwards the new
name and the other participants receive it as `group_changed`. It is therefore a
mutation, not a local label, and it is refused on a transport without groups.

`peer_requests`, `accept_peer_request` and `reject_peer_request` are the three
verbs of the friend-request vocabulary: list, answer yes, answer no. Until
`reject_peer_request` existed, a pending request could only be *accepted*, which
made the (bounded) list impossible to clear without becoming friends with whoever
had filled it; nothing is sent to the requester, because a request only becomes a
friendship when it is accepted.

`group_invites` is the queryable counterpart of the `group_invite_received` event.
An event is broadcast, so only a front-end that was already attached sees it; the
request is what lets a session that attached later still read the token and join
(same shape as `peer_requests` for friend requests).

`send_message` and `send_group_message` carry `kind` (`text` / `action`) and
`content_type` (`text` / `binary`); a binary body is lowercase hexadecimal, which
is how the protocol stays text-only while the transports carry bytes.

**Ordering guarantee.** Requests arriving on one connection are executed
strictly in arrival order, and their replies are emitted in that same order.
A client that sends `add_contact` followed by `list_contacts` therefore always
observes the addition. The server keeps this guarantee without blocking event
delivery by queueing requests onto a single dispatcher task per connection
(bounded queue, §3.7).

### 3.4 Replies

A response frame carries the correlation id and a *tagged* result: `status` is `ok`
with the payload under `reply`, or `err` with an `ErrorInfo` under `error`. The
failure tag is `err`, **not** `error`, and both literals are part of the contract:

```text
{"type":"response","id":1,"result":{"status":"ok","reply":{"type":"pong","echo":"hi"}}}
{"type":"response","id":2,"result":{"status":"err","error":{"code":"unauthorized","severity":"recoverable","message":"shutdown is reserved for the hosting process"}}}
```

Both are pinned by
`ipc_socket_test::test_the_wire_literals_are_what_a_foreign_client_reads`, which
reads the frames back as *bytes* rather than decoding them through the Rust types — a
`serde` rename would otherwise leave every other test green while breaking a client
written in another language. A refusal is a value on an otherwise usable session: the
same connection serves the next request (see §3.6).

`Reply` carries data only, never formatted text: `pong`, `session`,
`statistics`, `contacts`, `contact_added`, `contact_exists`, `contact_removed`,
`conversation`, `saved`, `history`, `updated`, `sent`, `shutting_down`,
`peer_requests`, `peer_request_accepted`, `peer_request_rejected`, `groups`,
`group_invites`, `group_created`, `group_joined`, `group_renamed`,
`group_invited`, `group_sent`, `group_left`, `metrics`.

`groups` and `group_invites` both *succeed* on a transport without groups, with
`supported: false` and an empty list, because "there are none" and "this transport
can never have any" are different answers and only the front-end can word the
difference.

### 3.5 Events

`CoreEvent` is broadcast, not queued per client, so a slow consumer cannot stall
the core: `ready`, `message_received`, `message_undecodable`,
`message_delivered`, `peer_connected`, `peer_disconnected`,
`conversation_changed`, `nickname_changed`, `status_changed`,
`peer_request_received`, `group_message_received`, `group_changed`,
`group_invite_received`, `shutdown`. A lagging socket subscriber is reported
server-side and skipped, never buffered without bound.

`group_changed` carries the group's id, name, member count and handshake state;
it is emitted when the group is joined, when a peer joins or leaves, and when a
peer renames it, so a front-end never has to poll to keep a name current. Its
`joined` flag is *reachability*, not "toxcore lists this conference": a conference
we created and nobody has joined is listed immediately but is not connected —
which is exactly why sending to it fails with `NO_CONNECTION`. It becomes true for
a created conference as soon as a peer is in it, or for a joined conference when
`conference_connected` fires; see A34.

`message_undecodable` is the honest counterpart of `message_received`: a frame
that decrypted successfully but cannot be shown is reported with its size, the
*reason* it was refused (`is not valid UTF-8 text` or `contains control
characters`) and, for a group message, the group it was sent to — instead of
being rendered as replacement characters, printed as it arrived, or dropped
without a word. It is not stored, so it cannot come back through `/history`
either.

### 3.6 Error model

`ErrorInfo { code, severity, message }` where `code` is a stable enum
(`invalid_request`, `not_found`, `network`, `cryptography`, `database`,
`configuration`, `unauthorized`, `unsupported_protocol`, `backpressure`,
`timeout`, `internal`) and `severity` is `info` / `recoverable` / `critical`.

Rules:

- Front-ends switch on `code`, never on message text.
- Only `internal` is critical; everything else leaves the session usable (R5,
  R6).
- An internal `MetaTextError` is flattened at the boundary because its boxed
  sources are not serializable; the full error stays in the backend log.

### 3.7 Boundary validation

Every value that enters the core is validated before it is used, so the domain
layer can rely on its invariants. Validation lives in `ipc::validation` and is
applied by `CoreService`, which means a socket client, a script and the TUI are
all subject to the same rules.

| Field | Rule | On violation |
| --- | --- | --- |
| nickname | 1–64 chars, trimmed, no control characters | `invalid_request` |
| status message | ≤ 256 chars, no control characters; empty clears it | `invalid_request` |
| contact identifier | 1–128 chars, no whitespace, no control characters | `invalid_request` |
| contact note | ≤ 256 chars, no control characters | `invalid_request` |
| message body | non-empty, ≤ `app.max_message_length` chars, control characters other than `\n` and `\t` rejected | `invalid_request` |
| peer address | `host:port` or `[v6]:port`, port 1–65535 | `invalid_request` |

Lengths are counted in `char`s, not bytes, so multi-byte input is measured the
way a user perceives it. Rejecting control characters keeps an escape sequence
out of another user's terminal and out of log records.

A rejected request is a pure failure: it changes no state, writes no row and
moves no counter. This is asserted by
`ipc_socket_test::test_boundary_validation_over_the_wire`.

**The same rule applies to what a *peer* sends**, which is the direction that
cannot be trusted at all — the table above guards a front-end against its own
user, while a peer is a stranger with the passphrase. Two kinds of string arrive
from the other end and both are printed:

- **A message body** is refused unless it is valid UTF-8 and carries no control
  character other than `\n`/`\t`, which is exactly the rule the send path applies
  (`core::body_of`). A refused body is reported as
  `CoreEvent::MessageUndecodable { reason, .. }` — with the group named when it
  arrived in one — counted and *not* stored, instead of being shown as it
  arrived or dropped in silence.
- **A label a peer chose** — the nickname in a TCP greeting, a Tox friend's
  announced name, a conference title, a participant's name — is refused unless it
  passes `ipc::validation::peer_label` (1–64 chars, no control character). Here a
  refusal cannot be reported as an event per occurrence (a Tox label is read with
  every snapshot), so the label is simply *not a name*: the connection, the friend
  entry and the counters stay, and the peer is identified by address or short key,
  exactly like a peer that announced nothing. The greeting is the one such string
  that *is* a discrete event, so its refusals are counted
  (`MetricsView::greetings_refused`).

Neither rule repairs the string. Trimming the control characters out of a body,
or keeping the printable part of a nickname, would display something the peer did
not send — the failure mode A11 closed for undecodable payloads — and it would
hide from the operator that something arrived that this instance refuses to show.

### 3.8 Limits

| Limit | Value | Where |
| --- | --- | --- |
| Frame size | 1 MiB | `ipc::protocol::MAX_FRAME_LEN` |
| In-flight requests | 64 | `CoreServiceOptions::command_capacity` |
| Events per subscriber | 256 | `CoreServiceOptions::event_capacity` |
| Transport → core queue | 1024 events, **bounded** | `network::EVENT_INBOX_CAPACITY` |
| Attached socket clients | 16 | `ipc::server` |
| Handshake time | 5 s | `ipc::server` |
| Per-client request queue | 128 | `ipc::server` |
| Per-client output queue | 128 frames | `ipc::server` |
| Per-client request rate | 256/s, burst 512 | `ipc::server::DEFAULT_REQUESTS_PER_SECOND` |
| Rate-limit strikes before detach | 256 | `ipc::server::RATE_LIMIT_STRIKES` |
| Tox outbox per friend | 32 payloads, 5 min TTL | `transport::TOX_OUTBOX_CAPACITY` |
| Tox queue file | ≤ 128 requests + 32 payloads per friend (same caps as above) | `tox_store::STORE_FILE` |
| Tox callback hand-off | 4096 events, drop-newest when full (counted) | `tox::EVENT_QUEUE_CAPACITY` |
| Pending friend requests | 128, drop-newest when full (counted) | `transport::PENDING_REQUESTS_CAPACITY` |
| Pending group invitations | 128, drop-newest when full (counted) | `transport::PENDING_INVITES_CAPACITY` |
| Pinned peer identities | 1024, drop-newest when full (counted) | `trust::PIN_CAPACITY` |
| Request deadline (remote) | 30 s | `ipc::client` |
| Message length | `app.max_message_length` (1372), hard ceiling 32 KiB | `ipc::core`, `config::MAX_MESSAGE_LENGTH` |
| History page | 1000 records, default 20 (or `--history-limit`, 1–1000) | `ipc::protocol::MAX_HISTORY_LIMIT` / `DEFAULT_HISTORY_LIMIT`, `CliArgs::history_limit` |
| Peer-supplied label | 1–64 chars, no control character; a refusal leaves the peer unnamed | `ipc::validation::peer_label` |
| Friend list | `app.max_friends` (1024) | `ipc::core` |

Bounding the transport → core queue means a peer that sends frames faster than
the core can absorb them is *throttled* (the transport awaits the send) rather
than growing an unbounded queue. Nothing is dropped, so no message is lost
under load.

### 3.9 Transport durability (Tox queue file)

Two things a user can observe are **not** in `toxcore`'s savedata, because
`toxcore` models neither as state: an incoming friend request (the request *is*
the callback; "not answered" is the refusal) and a payload buffered for an
offline friend. The Tox transport persists both in its own file next to the
savedata:

| Property | Value |
| --- | --- |
| Path | `<data-dir>/tox-state.json` (`tox_store::STORE_FILE`, next to `tox-savedata.bin`) |
| Layout | `{ "version": 1, "next_message_id": u64, "requests": [...], "outbox": [...] }` |
| Key | **public key**, never the friend number |
| Bounds | the transport's own caps: ≤ 128 requests, ≤ 32 payloads per friend |
| Timestamps | `queued_at_unix_ms` (a wall clock, so a payload can age across a restart) |
| Written | after every mutation, by atomic replace (temporary file, then rename), serialised between the actor and the bridge |
| Read | at startup; a missing, unreadable, corrupt or unknown-version file is an empty queue plus a warning |

Why keyed by **public key**: `toxcore` assigns a friend number from the order of
the savedata's friend list, so removing a friend renumbers the ones after it — a
file keyed by number would deliver a payload to the wrong peer after an unrelated
change. The transport translates keys back into the numbers of the current run
when it restores the queue, which is why the friend list (from the savedata) is
read before the queue is translated, and why a payload whose peer is not a friend
(any more) is dropped rather than guessed at.

Restoring applies the same rules as the runtime path: the TTL is measured on the
wall clock (a timestamp in the future is treated as *just queued*, because it
cannot be told apart from a clock that moved backwards, and delaying beats
dropping), a queue is capped per friend keeping delivery order, and a request is
deduplicated and capped. What is dropped is reported: expiries add to
`payloads_expired`, and the startup log names how many entries were undeliverable.

**What is not durable**: the TCP transport's outbox (`network`) is memory only — the
same restart loses it, and it is still keyed by the nickname a peer announced, which
is not an identity. §3.10's pin file and §3.11's key agreement remove two of the three
reasons that was so; the third is where the *sealing* happens (the core seals before
the transport sees the payload), and §6.17 records it. That asymmetry is deliberate.

### 3.10 The network identity, and the peers it pins

Every instance announces an identity to its peers (`FRAME_IDENTITY`, §3.11). Until
this change that value was `hex(random)` **per session**, which is exactly what
made the per-contact key half of A12 unverifiable: a key derived from a value that
changes every run separates two pairs but cannot be *checked* by either of them,
so it could never be pinned — and a queue cannot be keyed by a peer name that is
different every run (§6.17).

The identity is now a **persisted X25519 key pair**, one per data directory:

| Property | Value |
| --- | --- |
| Path | `<data-dir>/net-identity.json` (`identity::IDENTITY_FILE`) |
| Layout | `{ "version": 1, "secret": "<64 hex>" }` — the public half is derived from the secret, never stored twice |
| Announced | the **public half**, 64 uppercase hexadecimal characters (the shape the DID had, so no frame changed and a peer needs no new code) |
| Fingerprint | `SHA-256(public key)[..16]` in eight groups of four (`identity::NetworkIdentity::fingerprint`) |
| Written | when it is first generated, by atomic replace (temporary file, then rename), with `0o600` where the platform has modes |
| Read | at startup; a missing file is the first run (a new identity is generated *and written*), a corrupt, malformed or unknown-version file is logged and replaced |
| Key material | the secret half never leaves the process: not announced, not logged (the type's `Debug` is redacted), not in a session snapshot |

What the peer announced is remembered in a second file, so a change can be seen:

| Property | Value |
| --- | --- |
| Path | `<data-dir>/net-peers.json` (`trust::PIN_FILE`) |
| Layout | `{ "version": 1, "pins": [{ "nickname", "public_key", "first_seen_unix_ms" }] }`, sorted by nickname |
| Key | the **nickname** a peer announced, lowercased — the only handle TCP has (§6.17) |
| Bounds | ≤ 1024 entries (`trust::PIN_CAPACITY`), drop-newest when full, counted; an over-capacity or malformed file is read up to the bound and the excess is counted |
| Written | on a first sighting and on the **first change** of a nickname in a run (≤ 2 writes per nickname, i.e. bounded by the table rather than by a peer's frame rate), by the same atomic replace; a later change is reported and counted but kept in memory only, so a restart reports it again rather than missing it. A failed write costs the pin, not the session |

A change is **reported, not refused**: `peer_identities_changed` in `/metrics`,
one `warn` line naming both fingerprints, and `changed` on that peer's
`PeerIdentityView` in `/peers`. Refusing would not stop the peer — the value
travels in the clear, and any holder of the passphrase can claim a nickname — so
the honest design is to make the change visible and to leave the *refusal* to the
key agreement that closes A12. `peer_identities_refused` counts what could not be
pinned at all (no nickname, a key that is not 64 hexadecimal characters, a full
table), because a pin that was not kept cannot detect anything later.

**What the identity half buys, and what it does not.** It buys stability (the same
value across restarts), pinning (a change under a known nickname is detected) and
comparability (a user reads the fingerprint out and the peer reads theirs). What it
did not buy on its own is *secrecy from an observer*: the public half is announced in
the clear, so a passphrase holder who saw both values could derive the pair's key.
That is what the key agreement of §3.11 closes; this file is what makes the agreement
possible, because it is the static key pair the agreement signs nothing with but
authenticates through the pin.

### 3.11 The per-pair key agreement

The peer-to-peer frames are the transport's own contract, not the IPC protocol's:

| Frame | Payload | Sent |
| --- | --- | --- |
| `FRAME_HELLO` (1) | nickname, plain text | once per connection |
| `FRAME_MESSAGE` (2) / `FRAME_ACK` (3) | message id ‖ ciphertext / message id | per message |
| `FRAME_ACTION` (4) / `FRAME_BINARY` (5) | as `FRAME_MESSAGE`, meaning differs | per message |
| `FRAME_IDENTITY` (6) | the static public key, 64 hex | once per connection, in the greeting |
| `FRAME_EPHEMERAL` (7) | this connection's ephemeral public key, 64 hex | once per connection, right after the identity |
| `FRAME_KEEPALIVE` (8) | none | every `KEEPALIVE_INTERVAL` (20 s by default, `[network] keepalive_interval` may shorten it) while greeted |

Because these kinds are the *transport's* contract, adding one is not an IPC version
change: `PROTOCOL_VERSION` governs the front-end/core boundary, and a peer that does
not know a frame kind logs it as unknown and keeps working. That is what makes
`FRAME_KEEPALIVE` (and, before it, `FRAME_EPHEMERAL`) compatible with a peer that
predates it — the old end ignores the frame, and it also never sends one, which is
why the idle deadline it feeds only arms on a peer that has *shown* it sends them
(§6.20).

`FRAME_IDENTITY` used to carry a per-session random string that only served as a
public label. It now carries the persisted X25519 public key of §3.10, and the pair
key of a connection is agreed from a three-DH construction (RFC 7748) rather than
derived from what is public:

```text
  v1 = DH(S_own, P_peer)      static · static
  v2 = DH(E_a,   P_b)         ephemeral · static, a = the end with the lower static key
  v3 = DH(S_a,   E_b)         static · ephemeral
  key = HKDF-SHA256(session key, label "metaText/contact-key/v2", v1 ‖ v2 ‖ v3)
```

`DH(x, Y) == DH(y, X)`, so both ends compute the same three values; the order they are
concatenated in is pinned by the pair of static keys, which is why the exchange needs
no message beyond the two announcements each end already sends. The whole derivation
is `identity::pair_key`, and `identity::tests` pin its properties: symmetric, equal on
both ends, different per connection, and different from what an observer can compute.

| What the agreement buys | Why |
| --- | --- |
| A passive observer cannot derive a pair key | v1 needs a static **secret**; the session key (which every member has) and both announced public keys are no longer enough |
| A recorded connection does not open another | v2/v3 mix in an ephemeral that is new per connection |
| Membership of the session is still required | the session key is the HKDF salt, so a peer without the passphrase gets nothing extra |
| The key is bound to the two ends | every DH input is one of the two static keys or one of the two ephemerals |

**What it does not buy.** It is not a signature, so it cannot tell a *first* contact
whether the identity it is agreeing with is the one the human on the other side owns:
an active participant can present its own identity, agree a key, and relay. What it
cannot do is stay unnoticed — the identity is pinned (§3.10), so a later change is
reported, and two users who compare fingerprints out of band detect the substitution
immediately. Making the first contact itself resistant would need an identity
authenticated by something other than the channel (a signature with a key the peer
already trusts, or an explicit fingerprint confirmation), which is recorded as open
work rather than claimed here.

Two properties keep the failure modes of the exchange *closed* rather than quiet: an
ephemeral is taken **once** per connection (a repeated frame is ignored, so a
connection cannot be re-keyed under the feet of a peer that is still using the first
key), and a **tampered** ephemeral only ever costs the pair key — the frames from that
peer stop decrypting, which is a visible decryption failure, instead of silently
falling back to something weaker. An attacker on the path can suppress or corrupt the
frame, and that is the same "active participant" case the paragraph above describes.

**The three-step key ladder.** A directed message is sealed under the first key that
exists, and a receiver tries them in the same order (`network::pair_keys`):

1. the **agreed** key (v2 label) when both ends announced an identity *and* an
   ephemeral for this connection;
2. the **static derivation** (v1 label) when both announced an identity but no
   ephemeral — a peer from before the agreement, or one whose ephemeral frame was
   missing or unusable;
3. the **session key** when the pair has no key of its own: the peer is not connected,
   it announced no identity, or this session has none. A payload buffered for an
   offline peer is sealed this way, because there is no connection to agree a key on.

The steps are domain-separated by their labels, so a v1 key can never be mistaken for
a v2 one. An announcement that is not a key is treated as *no* announcement (never as
a weaker scheme). An on-path attacker that suppresses the ephemeral frame can still
force step 2 — but such an attacker holds the session key anyway, and what it loses by
doing so is the ability to stay passive: step 2 is a key that anybody who watched the
handshake can derive, so the traffic it has to touch is no longer *only* touchable by
the two ends.

Both halves of the rule live on the transport: `NetworkManager::contact_key` is what the
core asks for before it seals a payload, and `NetworkManager::send_to_peer` — the
convenience that takes *plaintext* — applies the same ladder rather than sealing
everything with the session key, so a caller cannot downgrade a directed message by
picking the easier entry point.


## 4. Design controls

An aerospace-style argument is: every requirement is discharged by a control
that is verified by evidence. The table maps requirement → control → test.

| ID | Control | Evidence |
| --- | --- | --- |
| R1 | `--mode core`/`server`/`daemon` and `--headless` run `CoreService` with no interface; `CoreService::new` performs no I/O | `repl_commands_test::test_headless_suppresses_repl` |
| R2 | One `Request`/`Reply` vocabulary; `CoreClient` is the only handle a front-end gets | `core_service_test` (all) |
| R3 | Presentation compiles without subsystem crates in scope; the actor owns all mutable state | module layout + `core_service_test::test_contact_lifecycle` |
| R4 | `LocalClient` and `RemoteClient` implement the same trait over the same messages | `ipc_socket_test::test_remote_client_roundtrip` |
| R5 | Typed `ErrorInfo`; only `internal` is critical | `protocol::tests::test_error_severity_mapping` |
| R6 | Failures are values returned to the caller; the actor loop continues | `core_service_test::test_failures_do_not_poison_the_service` |
| R7 | Table in §3.7; bounded queues/frames/connections | `framing::tests::test_oversized_frame_is_rejected`, `core_service_test::test_command_queue_applies_backpressure` |
| R8 | Actor serialisation; dependency-injected paths; temp-dir tests | `core_service_test::test_session_persistence_roundtrip` |
| Security | Constant-time token compare; remote shutdown refused; size-before-allocation | `server::tests::test_secret_comparison`, `ipc_socket_test::test_remote_shutdown_is_refused`, `ipc_socket_test::test_wrong_token_is_refused` |
| R9 (input integrity) | Every boundary value validated before use; rejected requests have no side effects | `ipc::validation` tests, `ipc_socket_test::test_boundary_validation_over_the_wire` |
| R10 (no silent loss) | Undecodable payloads reported, not mangled | `core::tests::test_undecodable_message_is_reported_not_mangled` |
| R11 (bounded memory) | Bounded transport queue with backpressure; bounded per-client queues; bounded per-peer frame queue that sheds and counts; bounded greeting deadline that reclaims a silent socket's slot; idle deadline that reclaims a half-open one | `core_service_test::test_command_queue_applies_backpressure`, `network::EVENT_INBOX_CAPACITY`, `network::tests::test_a_peer_that_stops_reading_sheds_instead_of_growing`, `network::tests::test_a_peer_that_never_greets_loses_its_slot`, `network::tests::test_a_silent_peer_is_dropped_once_it_has_proved_it_sends_keepalives` |
| R12 (config integrity) | `AppConfig::problems()` rejects an unusable file before any socket is bound; configured defaults re-validated inside the library | `config::tests::test_problems_cover_every_section`, `core::tests::test_invalid_configured_defaults_are_not_applied` |
| Change control | `PROTOCOL_VERSION` negotiated per connection | `ipc_socket_test::test_protocol_version_is_announced`, `test_stale_protocol_version_is_refused` |
| Robustness | Deterministic corpus tests: arbitrary frames round-trip, arbitrary garbage is handled, one-byte-at-a-time delivery reassembles | `ipc::framing` tests |
| Availability | A hostile client cannot occupy an endpoint slot forever or break it for others | `ipc_socket_test::test_silent_client_is_dropped_by_the_handshake_timeout`, `test_oversized_frame_does_not_break_the_endpoint` |
| R13 (transport independence) | `CoreService` owns a `CoreTransport`; front-ends never name a transport | `tox_core_transport_test` (all) |
| R14 (transport integrity) | A Tox address is only dialled after toxcore accepts it; a corrupt checksum is reported, not silently ignored | `tox::tests::test_corrupt_address_is_rejected`, `tox_core_transport_test::test_tox_connect_validates_the_address_shape` |
| R15 (no corruption) | Tox message bodies are carried and decoded as bytes, never lossily | `tox::tests` + `ToxEvent::text` strictness |
| R16 (Tox identity durability) | Identity lives in toxcore savedata and survives a restart | `tox_core_transport_test::test_tox_identity_is_stable_across_restarts` |
| R17 (bootstrap integrity) | `--bootstrap host:port:KEY` is shape-checked before `tox_bootstrap` sees it | `tox::hex_tests::test_bootstrap_node_parse` |
| R18 (explicit trust) | A friend request is surfaced, never auto-accepted | `tox_core_transport_test::test_tox_peer_requests_start_empty` |
| R19 (bounded CPU) | A per-client-**address** token bucket sheds a flood with `backpressure`; a persistent flooder is detached; the shed reply keeps request order; the allowance follows the address, so reconnecting does not reset it | `server::tests::test_token_bucket_*`, `server::tests::test_a_budget_follows_the_address_not_the_connection`, `ipc_socket_test::test_request_rate_limit_sheds_a_flood`, `ipc_socket_test::test_a_reconnect_does_not_refill_the_budget` |
| R20 (no accidental exposure) | `--ipc-listen` on a non-loopback address without `--ipc-token` refuses to start | `repl_commands_test::test_non_loopback_endpoint_requires_a_token`, `utils::tests::test_is_loopback_host` |
| R21 (Tox delivery parity) | A payload for an offline Tox friend is buffered (bounded, expiring) and flushed when the friend connects, exactly like TCP | `tox_core_transport_test::test_tox_add_contact_sends_a_friend_request`, `test_two_cores_exchange_a_message_over_tox` |
| R22 (message integrity) | Two cores exchange messages: correct attribution, sender order, byte-exact multi-byte and near-limit bodies, both directions on one connection, offline buffering then flush | `message_exchange_test` (all) |
| R23 (payload kind) | A `/me` action keeps its kind across the transport (TCP frame kind, `TOX_MESSAGE_TYPE`) and is rendered as an action, never as a chat line | `message_exchange_test::test_action_messages_keep_their_kind`, `repl_commands_test::test_me_action_is_sent_and_rendered`, `tox::hex_tests::test_message_kind_round_trips` |
| R24 (independent upgrade) | The endpoint serves every version in `MIN_SUPPORTED..=PROTOCOL_VERSION` and rejects the rest with a message naming the window | `ipc_socket_test::test_the_compatibility_window_is_served`, `test_stale_protocol_version_is_refused` |
| R25 (observability) | `Request::Metrics`/`MetricsView` exposes queue depths, sheds, expiries, subscribers and traffic as one non-blocking snapshot | `message_exchange_test::test_metrics_reflect_the_exchange`, `test_metrics_track_a_buffered_payload`, `ipc_socket_test::test_metrics_are_available_over_the_wire`, `repl_commands_test::test_run_executes_all_repl_commands` (`/metrics`) |
| R26 (bounded hand-off) | The toxcore callback queue is bounded; a shed event is counted instead of growing memory, and the counter is visible in `/metrics` | `tox::tests::test_a_quiet_instance_sheds_nothing`, `tox::hex_tests::test_event_queue_capacity_is_bounded_but_generous` |
| R27 (history fidelity) | A message kind and content type survive persistence; a legacy database is upgraded in place and old rows decode as ordinary text messages | `database::tests::test_message_kind_round_trips_through_the_database`, `test_migration_adds_the_kind_column_to_a_legacy_table`, `message_exchange_test::test_received_messages_are_in_the_history_after_a_restart` |
| R28 (content type) | A binary payload keeps its type end to end: it is never decoded as text, never reported as undecodable, and arrives byte-exact; a text body is never silently reclassified | `message_exchange_test::test_binary_payload_round_trips`, `network::tests::test_message_frame_kinds`, `ipc::validation` tests (`test_binary_rules`) |
| R29 (contradiction refusal) | `(action, binary)` and a malformed or oversized binary body are refused before any work, with no side effects | `message_exchange_test::test_invalid_binary_body_is_refused_without_side_effects` |
| R30 (actor lag) | Queue wait and service time per request are measured by the actor and reported; a queued request cannot report a zero wait | `core_service_test::test_metrics_report_actor_lag` |
| R31 (group chat) | A group can be created, listed, sent to and left through the same protocol and the same core state as a direct conversation, over the transport that supports it | `tox_core_transport_test::test_group_lifecycle_over_the_tox_transport`, `tox::tests::test_conference_lifecycle_without_a_dht` |
| R32 (capability honesty) | A transport without groups says so: the capability is advertised in `SessionInfo`/`MetricsView`, the list request succeeds with `supported: false`, and every mutation is refused with a reason and a remedy | `core_service_test::test_groups_are_reported_as_unsupported_on_the_tcp_transport`, `repl_commands_test::test_run_executes_all_repl_commands` (`/group`) |
| R33 (group input integrity) | A group body obeys exactly the same limits as a direct one; an unknown group, peer or token is refused before anything is sent, with no side effects | `tox::tests::test_conference_input_is_validated`, `core::tests::test_group_selector_resolution`, `core_service_test::test_groups_are_reported_as_unsupported_on_the_tcp_transport` |
| R34 (group history fidelity) | A group message is persisted with its conversation kind and group id, and renders with the group's name; a legacy database decodes as a direct conversation | `database::tests::test_group_message_round_trips_through_the_database`, `test_migration_adds_the_kind_column_to_a_legacy_table`, `core::tests::test_group_message_is_published_counted_and_persisted` |
| R35 (inbound display integrity) | Nothing a *peer* sends is printed unchecked: a text body must be valid UTF-8 and free of the control characters a body may not carry (the rule the send path already applied), and every peer-chosen label (greeting nickname, Tox friend name, conference title, participant name) must pass the label rule. A refused body is reported as `message_undecodable` with its reason (and its group) and is not stored; a refused label leaves the peer unnamed while its connection, frames and counters stay | `core::tests::test_a_body_with_control_characters_is_reported_not_displayed`, `test_newlines_and_tabs_still_survive_in_a_body`, `test_a_group_body_with_control_characters_is_reported_with_its_group`, `test_non_utf8_group_text_is_not_published`, `network::tests::test_a_greeting_with_an_unusable_nickname_is_refused`, `test_a_message_body_reaches_the_core_verbatim`, `tox::tests::test_a_conference_title_that_cannot_be_displayed_is_refused`, `ipc::validation` tests (`test_peer_label_accepts_only_displayable_names`) |

Additional invariants enforced by construction:

- **No undocumented public item** (`#![deny(missing_docs)]`) and **no `unsafe`
  outside the FFI module**: every crate declares `#![deny(unsafe_code)]`, and the
  one exemption is `tox.rs`, which opts out with `#![allow(unsafe_code)]` because it
  is a hand-written binding. That module's 55 `unsafe` blocks each carry a `SAFETY`
  argument (audited: none without one), every slice built from a callback argument
  is guarded by `is_null() || length == 0` before `from_raw_parts` — the case the
  standard library calls undefined even for a zero length — and the opaque context
  pointer is checked null before it is dereferenced.
- **No panics on the interface path**: malformed input becomes `ErrorInfo` or an
  I/O error, a fault travels to the user as an error value, and a front end that
  loses a thread reports it instead of unwinding. This is a *gate*, not a habit: the
  crates deny `clippy::unwrap_used`, `expect_used`, `panic`, `unreachable`, `todo`
  and `unimplemented` in the shipped paths (a `cfg(test)` allow keeps tests able to
  assert by unwrapping) and the CI lint step passes `-D warnings`, so a new
  `unwrap` in the tree fails the build. An audit of the shipped paths finds **no
  `unwrap()`, `panic!`, `unreachable!`, `todo!` or `unimplemented!`** at all; the two
  remaining `expect`s were the stdin-reader thread spawns, which now return the
  failure, and the one index that peer input could reach (the message-id split) is a
  checked slice.
- **No warnings accumulate**: `rustfmt --check`, `clippy -- -D warnings`,
  `cargo doc` with `RUSTDOCFLAGS=-D warnings` and the test suite are all part of the
  pipeline (`make ci` runs the same four), so a lint that becomes noisy is a build
  failure rather than a comment in a review.
- **No connection slot without a bound**: a peer has the configured
  `connection_timeout` to announce itself, and a socket that never does is closed
  (`NetworkManager::read_loop`), so `max_connections` silent sockets cannot make the
  instance unreachable. Each connection's frame queue is bounded too
  (`PEER_QUEUE_CAPACITY`): a peer that stops reading sheds and counts
  (`payloads_dropped`) instead of growing the sender's memory. Once a peer has proved
  it sends keepalives, silence for `IDLE_TIMEOUT` closes the connection as well, which
  is what turns a half-open socket into a slot the endpoint can reuse (§6.20).
- **Secrets never logged**: `CoreService` implements a redacting `Debug` and the
  session identity is not key material.

## 5. Audit — findings and disposition

The audit that preceded the refactor found the following. "Closed" means the
refactor removed the defect; "open" means it is recorded for follow-up.

| # | Finding (before) | Impact | Disposition |
| --- | --- | --- | --- |
| A1 | `MetaTextApp` owned the TUI, the crypto, the database and the network, and interleaved `println!` with domain logic | UI and backend could not evolve independently; the backend could not run headless as a service | **Closed**: `CoreService` owns the subsystems, `ui::presenter` owns the wording |
| A2 | No serializable interface, so no out-of-process front-end was possible | An embedding host had to link the whole client | **Closed**: versioned JSON protocol + `ipc::server` |
| A3 | Domain state guarded by `Arc<RwLock<..>>` shared with the UI | Front-ends could observe torn state and mutate invariants | **Closed**: single-owner actor |
| A4 | `static OUTPUT_SINK` plus direct `println!` inside domain code | Untestable, and output could corrupt the alternate screen | **Closed**: the engine routes output; the core never writes to stdout |
| A5 | Unbounded request channels | Unbounded memory growth under load | **Closed**: bounded queues with explicit backpressure |
| A6 | `anyhow` errors crossed the (implicit) UI boundary | Untyped, unserializable failures | **Closed**: `ErrorInfo` with stable codes |
| A7 | The TUI engine depended on `AppEvent` (a domain type) | Rendering could not be reused or tested in isolation | **Closed**: `TuiInput` / `TuiInfo` only |
| A8 | No authentication on any transport boundary | Any local process could drive the backend once exposed | **Closed**: token handshake, constant-time compare |
| A9 | `--mode server`/`daemon` had no endpoint | The modes were effectively no-ops | **Closed**: `--ipc-listen` / `--ipc-token` |
| A10 | The network → core channel was an unbounded `mpsc` | A peer flooding frames could grow the queue without bound | **Closed**: the channel is bounded (`EVENT_INBOX_CAPACITY`) and the transport awaits it, so overload throttles the sender instead of growing memory |
| A11 | `MessageReceived` decoded the payload lossily to UTF-8 | A non-text payload was displayed as U+FFFD, i.e. as content that was never sent | **Closed**: decoding is strict and a failure produces `CoreEvent::MessageUndecodable` (protocol v2) |
| A12 | Contact identifiers are placeholders, not real keys | No per-contact key material at all: one session key served every frame, so any peer that knew the passphrase could read a message addressed to somebody else, and a frame from one pair could be replayed as another pair's | **Closed.** A directed message is sealed under key material of the pair's own, and the two halves are both in place. The **agreed** half (§3.11): each connection contributes an ephemeral X25519 key pair and derives HKDF-SHA256(session key, `…/v2`, DH(S,S)‖DH(E,S)‖DH(S,E)), so the key needs a static *secret* — holding the passphrase and having seen both announced public keys (which is what an observer of the handshake has) is no longer enough, and a recorded connection does not yield the key of another. The **identity** half (§3.10): the announced value is a persisted X25519 public key, so it is stable across restarts, a peer pins it and a change under a known nickname is reported, and a user compares fingerprints out of band. A captured frame is therefore bound to the two ends that wrote it, unreadable to a passive member, and a substitution is *reported* (not silent). What is *not* claimed: a first contact is trust-on-first-use, so an active participant that presents its own identity can still relay — that needs an identity authenticated off-channel, and it is the open item §6.2 now names instead of the whole finding |
| A13 | Crate is still a single Cargo package with internal module boundaries | The boundary is enforced by convention and review, not the compiler | **Open**: see §6 |
| A14 | Configuration was read but not validated as a whole, and several values were ignored (`connection_timeout`, the whole `[logging]` section, bootstrap addresses) | A typo in the file surfaced late or silently; an operator could not actually change the log destination | **Closed**: `AppConfig::problems()` covers every section, `main` fails fast, and the transport/logging now honour their configuration |
| A15 | User input was only length-checked for messages | A nickname or identifier could be unbounded, contain NUL, or carry terminal escape sequences into another user's screen | **Closed**: `ipc::validation` enforces per-field rules at the boundary |
| A16 | Tox was served by a second, parallel front-end (`ui::tox`) that owned the instance and re-implemented addressing, persistence and the command set | Tox users got no message history or contacts database, no TUI, and two copies of the command wording could drift apart | **Closed**: `transport::CoreTransport` lets `CoreService` own either transport, so Tox reuses the contact list, history, `/info`, the REPL and the TUI |
| A17 | The Tox event stream was decoded lossily (`String::from_utf8_lossy`) | A binary payload would have been silently corrupted into U+FFFD | **Closed**: `ToxEvent::Message` carries raw bytes and `ToxEvent::text` only decodes strictly when a caller asks for text |
| A18 | `--bootstrap` was rejected outright for Tox, so a private DHT node could not be used without rebuilding | An operator could not point the client at their own infrastructure | **Closed**: `--bootstrap host:port:PUBLIC_KEY` is parsed (`BootstrapNode::parse`) and validated on the command line |
| A19 | Incoming friend requests had no representation in the protocol | The Tox REPL could accept them but no other front-end could, and the request was only visible as a raw event | **Closed**: protocol v3 adds `PeerRequests` / `AcceptPeerRequest` / `PeerRequestReceived`; `/requests` and `/accept` are shared by CLI and TUI |
| A20 | The bounded queues limited *memory*, not the work a socket client could make the core do | A client could keep the actor busy with requests it had no intention of waiting for, starving other front-ends | **Closed**: a per-client token bucket (`ipc::server::TokenBucket`) sheds with `backpressure` and detaches a client that keeps flooding |
| A46 | The request limiter was per *connection*, and its policy was only reachable from code | The bucket lived on the socket, so the cheapest possible bypass was to reconnect: a flood that was shed could start again on the next connection, and a loop was free. `ServerOptions::requests_per_second` / `request_burst` existed but were set by nobody — `main` built the endpoint with `..ServerOptions::default()`, so an operator had no way to tighten or loosen the limit for the interface they were exposing. Same failure mode as A42/A45: policy that exists in code but not where the operator can reach it | **Closed**: `ipc::server::PeerBudgets` keys the budget by client address (a reconnect inherits it, another address gets its own), bounded by an idle TTL and by evicting the least recently seen address; the policy is in the `[ipc]` config section with `--ipc-rate` / `--ipc-burst` overrides; the headless start logs the limit. The socket test that used to assert the *old* contract ("a new client has its own bucket") now asserts the new one, and a dedicated reconnect test pins it. See §6.6 |
| A21 | `--ipc-listen` on a public interface without `--ipc-token` only logged a warning | An unauthenticated endpoint could be exposed to the network by a typo, letting any host drive the backend | **Closed**: startup is refused unless the address is loopback or a token is set (`utils::is_loopback_host`) |
| A22 | A Tox message to an offline friend was reported as `dropped` | Tox sessions silently lost messages that the TCP transport would have buffered, so the two transports disagreed on what "sent" means | **Closed**: `ToxTransport` keeps a bounded, expiring per-friend outbox and flushes it when the friend connects |
| A23 | The toxcore callback → bridge channel was unbounded (`std::sync::mpsc::channel`) | A peer sending faster than the front-end renders grew that hand-off without limit, i.e. the one unbounded queue left in the design | **Closed**: it is a bounded `sync_channel` (`tox::EVENT_QUEUE_CAPACITY`) with `try_send`; a shed event is counted and reported as `payloads_dropped` in `/metrics` |
| A24 | The only operational counters were `/stats` (traffic only) | An operator could not tell "quiet" from "stuck": no queue depth, no shed count, no subscriber count | **Closed**: `Request::Metrics` / `MetricsView` / `/metrics` expose all of them as a non-blocking snapshot |
| A25 | History stored only the body | A `/me` action was restored as an ordinary message, so a replayed history did not read the way the session did | **Closed**: `messages.kind` is persisted, exposed in `MessageView`, rendered as `* <peer> <action>`, and an older database is upgraded in place by a guarded `ALTER TABLE` |
| A26 | Only UTF-8 text could be sent, and a non-UTF-8 payload was always reported as undecodable | A peer that *intended* to send bytes was indistinguishable from a corrupt frame, so the user saw a warning for an expected payload and could not send one at all | **Closed**: `ContentType::{Text, Binary}` travels end to end (a TCP frame kind, sniffed on Tox because toxcore carries no type, persisted as `messages.content_type`); a binary body is rendered by size and preview and is never a decode failure |
| A27 | A reply channel could not tell "quiet" from "stuck" | `/metrics` reported traffic and queue depth, but not how long a request waited, which is the signal that actually distinguishes a busy actor from a dead one | **Closed**: `Command` carries an enqueue `Instant`; the actor records queue wait and service time (last and worst) plus a served count in `MetricsView` |
| A28 | Group chat was *declared* but not implemented: `AppEvent::GroupEvent` with a `Uuid` group id and a `GroupEventType` enum had no producer anywhere, and the core ignored them | A reader (or a generated binding) would believe group events could arrive; the types were a lie that also fixed the wrong identity, because a locally invented `Uuid` cannot identify a transport's group | **Closed**: the dead scaffolding is replaced by the real vocabulary — `AppEvent::{GroupChanged, GroupMessageReceived, GroupInviteReceived}`, `Group` keyed by the transport's stable id, and `Request::{Groups, CreateGroup, JoinGroup, InviteToGroup, SendGroupMessage, LeaveGroup}` over Tox conferences |
| A29 | Every group failure was reported as a network error | A front-end would look for a connection problem when the real reason was "this transport has no groups" or "that group does not exist" | **Closed**: the core derives the code from the underlying error and only decorates the message with the operation; the capability check runs before state lookup, so the answer names the actual reason and the remedy |
| A30 | A Tox conference rename was invisible: `tox_conference_title` was never registered | The group kept its old name in every front-end until the next `/group list`, so two participants could disagree about a group's name indefinitely | **Closed**: the callback is registered and folded into the existing `GroupChanged` vocabulary (`tox_conference_title` → `ToxEvent::ConferenceTitleChanged` → `AppEvent::GroupChanged`), which needs no protocol change because `group_changed` already carries the name |
| A31 | `connected_peers` counts a peer the moment its socket is registered, but TCP routes a message by *nickname*, which arrives in the peer's `hello` frame | A client that watched `connected_peers` (or `/metrics`) and sent immediately could be told `queued` for a peer the UI called connected; correct and lossless — the payload is flushed when the hello lands — but the readiness signal did not mean what it said | **Closed**: the metric is documented as the transport's connection count (with `peer_nicknames` naming the addressable subset), and the core-to-core tests wait for the announced nickname, not the socket; the race was showing up as two intermittently failing `message_exchange_test` cases |
| A32 | Two group-history tests read from the database without a `sqlite` gate, while every sibling persistence test had one | `cargo test` (default features) — the first command CI runs — failed with `Persistence requires a build with the sqlite feature`, so a red default build could be mistaken for a broken feature build | **Closed**: the driver-dependent assertions are behind `sqlite` (the core test still checks the event, counters and state in every build), and the group tests are listed in §7 so the two feature builds are both accounted for |
| A33 | A group invitation could only be learned from the `group_invite_received` event, while `SessionInfo` advertised how many were pending | The count had no readable counterpart: a front-end that attached after the event (an out-of-process UI, a reconnect) could see "1 invitation" and had no way to obtain the token, so it could never join. The friend-request side had already solved exactly this with `Request::PeerRequests` | **Closed**: protocol v6 adds `Request::GroupInvites` / `Reply::GroupInvites` (reusing `GroupInviteView`), `/group invites` prints each token on its own line, and the ignored DHT test reads the token from the *request*, not the event |
| A34 | `tox::ToxClient::snapshot` reported every conference as `connected: true` while `conference_new` reported `false` for the same conference | Two answers from one instance contradicted each other and the `connecting` state was unreachable: a peerless conference — which cannot receive a message at all (`NO_CONNECTION`) — was displayed as joined | **Closed**: `connected` is derived instead of asserted — `conference_connected` (recorded in `CallbackCtx::connected`) **or** more than one peer. The callback alone would be wrong: tox.h documents it as firing only "after joining [a conference] with `tox_conference_join`", and the DHT test confirmed the *creator* never receives it (both sides at 2 peers, only the joiner at `joined: true`), so a created conference is connected once somebody is in it |

| A35 | `ToxTransport::join_group` removed the invitation *before* attempting the join | A transient failure (`FAIL_SEND`, a friend momentarily unreachable) consumed a token that had not been used, so a retry was impossible and the peer had to re-invite; the documented single-use rule had silently become "single-attempt" | **Closed**: the token is spent by a join that succeeded (the audio/video refusal still consumes it, because nothing can ever use it); replay is still impossible. Covered by the ignored DHT test, which joins once and then asserts the same token is refused |
| A36 | The pending friend-request and group-invitation lists had **no bound at all** | A friend request needs only our Tox address to be produced, so anyone can create one entry per identity they generate: the list (and with it the process) grew on a remote party's schedule — the one class of growth R7 exists to prevent, and the reason the event queue was bounded in A23 | **Closed**: both lists are capped (`transport::PENDING_REQUESTS_CAPACITY` / `PENDING_INVITES_CAPACITY`, 128) with drop-newest so a flood cannot evict a request the user is about to accept, and the refusals are counted (`/metrics`: `friend_requests_dropped`, `group_invites_dropped`) instead of vanishing. A duplicate is not counted, since a repeated request is not a refusal |
| A37 | `ToxTransport::accept_request` removed the request before calling toxcore, and two caches only ever grew | A refused accept (for example a full friend list) lost the request, so a retry needed the peer to send it again — the same mistake as A35; and `group_names` plus the conference-handshake set kept an entry per group ever joined, including ones that were left | **Closed**: the request is spent after a successful accept; the title cache is cleared on leave and the handshake set is pruned against toxcore's current conference list, so both stay proportional to what is actually joined |
| A38 | A pending friend request could only be **accepted** — there was no reject — while the list that holds them is bounded (A36) | The two together were worse than either: a flood filled the list, and the only way to make room was to become friends with the flooders, after which the attacker refills it. A basic verb every client needs was missing, and its absence turned the new bound into a wedge | **Closed**: `Request::RejectPeerRequest` / `Reply::PeerRequestRejected`, `/reject <index\|public key>` (alias `/decline`), and `ToxTransport::reject_request`. Nothing is sent to the requester — toxcore has no reject message, and a request becomes a friendship only when accepted — so the observable effect is that the entry is gone and no contact was created. Both the yes and the no path are exercised over the real DHT (`test_two_cores_exchange_and_refuse_a_friend_request_over_tox`), since an offline test cannot produce a pending request |
| A39 | `app.max_friends` was enforced on `add_contact` only, while accepting a request created a contact unchecked | The documented limit (`§3.8 "Friend list"`) was not an invariant: a session could grow past it by accepting requests, so two paths produced the same state with different rules. A limit that holds on one of two paths is a limit the user cannot rely on | **Closed**: one `friend_limit_error()` is consulted by every path that creates a contact, checked *before* the transport so a refused accept leaves the request pending (it can be answered after the user makes room). Both paths are asserted in `core::tests::test_the_friend_limit_is_enforced_when_accepting_a_request` |
| A40 | A pending group invitation can only be **joined**; there is no counterpart to `Request::RejectPeerRequest` that discards one, while the list that holds them is bounded (A36) | The same *accept-only* asymmetry A38 closed on the friend-request side, left open on the invitation side: `Request::GroupInvites` can list an invitation that the protocol offers no way to answer *no*. It was milder than A38 — an invitation is a local capability token, not a relationship, joining and then `/group leave` consumes the token and clears the pending entry, nothing is sent back to the inviter, and the overflow is already counted (`/metrics`: `group_invites_dropped`) — but "the only way to make room is to accept" is the same wedge, and it put the burden of a workaround (`join` + `leave`) on the user | **Closed**: protocol v7 adds `Request::DeclineGroupInvite` / `Reply::GroupInviteDeclined`, `CoreTransport::decline_group_invite` / `ToxTransport::decline_group_invite` (a local removal of the pending token — toxcore has no decline message), `/group decline <token>` (alias `/group reject`), and the help/README wording. A decline is deliberately *not* a block: the inviter can invite again. Covered without a DHT by `tox_core_transport_test::test_group_lifecycle_over_the_tox_transport` (empty-token refusal, unknown-token `Network` error, no state change) and `core_service_test::test_groups_are_reported_as_unsupported_on_the_tcp_transport` (typed refusal on TCP), end to end by the opt-in DHT test, which now declines a real invitation, proves the token can no longer be joined or replayed, proves no group was entered, and then joins the invitation the same peer sends again (whose cookie may be identical — see §5.1) |
| A41 | A `ToxTransport` dropped **without** `shutdown()` left its bridge task running | The bridge is a `spawn_blocking` task, and a tokio runtime waits for its blocking tasks when it is dropped; the stop flag was set only by the orderly `shutdown()`, so every other teardown path — an actor dropped by a runtime, or a front-end/test that panicked before its `shutdown().await` — left the bridge looping in `ToxClient::next_event()` and the runtime waiting on it. The failure mode is a **hang, not an error**: teardown never returns, which is how a panicking opt-in DHT test wedged the whole test binary. Observed, not inferred: a stack sample of the hung process showed the main thread parked in `BlockingPool::shutdown` while two `spawn_bridge` tasks sat in `next_event()` | **Closed**: `impl Drop for ToxTransport` sets the bridge's stop flag, which the loop already re-reads every `TOX_EVENT_POLL` (200 ms), so the bridge exits and writes the savedata on its way out; `shutdown()` remains the orderly path and additionally joins the task. Covered by `transport::tests::test_dropping_the_transport_stops_its_bridge`, which awaits the bridge under a bounded `timeout` so a regression *fails* the suite instead of hanging it (verified to fail with the flag write removed) |
| A42 | Two `[network]` switches were accepted by the parser and read by nothing | `enable_upnp` and `enable_ipv6` appeared in `config.toml` and `NetworkConfig` and passed validation, while no code read them: the listener was always the IPv4 wildcard and no port mapping was ever attempted. A14 claims "the whole file is validated", which is not "every key changes behaviour" — a key the binary ignores is a promise the file cannot keep, exactly what A14 set out to remove | **Closed**: `enable_ipv6` is enforced — the TCP listener binds one dual-stack `[::]` socket (`socket2`, because `std` cannot clear `IPV6_V6ONLY` and a default IPv6 wildcard is v6-only on Windows) with an IPv4 fallback, and the Tox transport passes the flag to toxcore's `ipv6_enabled`; `enable_upnp` takes this finding's other sanctioned route — `true` is rejected by `AppConfig::problems()` and the default/template ship `false`, because no UPnP mapping client is linked. Covered by `network::tests::test_ipv6_listener_is_dual_stack`, `tox::tests::test_ipv6_switch_is_applied_to_the_instance`, and the `network.enable_upnp` row in `config::tests::test_problems_cover_every_section` |
| A43 | A Tox friend whose nickname arrived after `friend_connection_status` stayed nameless for the whole session | `tox_callback_friend_name` was not registered and the friend table was refreshed *only* by the connection callback (`transport::forward_event`), so when toxcore raised the name second there was no path that ever learned it: `/peers` printed "(+1 still completing the handshake)" indefinitely, an inbound message was attributed to `short_key(public_key)`, and `/msg <nickname>` failed with "does not match a friend of this Tox instance". Observed on one side of a mutual add, reproducibly, by polling `/peers` every 10 s for 400 s on both instances — the other side had the name, which is what identified the ordering as the cause rather than a missing name in the protocol | **Closed**: `tox_callback_friend_name` is registered and raises `ToxEvent::FriendName`; the bridge refreshes the table from the snapshot and announces the newly learned name through `reported_nickname`, which reports it **once** and only for a *reachable* friend — so a reconnect (name re-learned) does not print a second "connected" line, and a rename while offline updates the table silently. Message attribution refreshes a still-unnamed friend before labelling a message, so an in-flight event cannot mis-attribute it. Covered by `transport::tests::test_a_late_nickname_is_reported_once`, which pins both event orders |
| A44 | `README` documents `port 0` as supported, both validators reject it, and every integration test relies on it | `CliArgs::validate` ("Port cannot be 0") and `AppConfig::problems` ("network.port cannot be 0") rejected the value, while the `README` explained `/peers` as "useful when port `0` was configured (the OS picks the port)" and every test helper started a core with `port = 0` (validators are only run by `main`, so the tests never noticed the contradiction). The code paths themselves always supported it: the listener binds an ephemeral port, toxcore is passed `0` when no port is pinned, and `/peers`/`/whoami` report the bound value | **Closed by honouring the documentation**: port `0` is accepted by both validators and documented as "the OS picks a free port" (CLI help, config doc comment, README option table). The old rule was the one that could not be satisfied by any user — a documented capability unreachable from the command line. Covered by `cli::tests::test_validation`, `main::tests::test_main_argument_validation` and `config::tests::test_port_zero_means_the_os_picks_a_free_port` |
| A45 | `logging.rotation_size_mb` was validated and then read by nothing | `AppConfig::problems` rejects `0` (so the key is a first-class setting), the shipped `config.toml` sets `10`, and `main` even logged "ℹ️ logging.rotation_size_mb is not enforced by the time based appender" — a runtime confession that the file promised a bound it did not apply. `tracing-appender` only rotates by time, so a long-running peer's day file grew without limit while an operator who set the key believed otherwise. Same failure mode as A42, on the logging side | **Closed**: `logging::SizeRotatingWriter` enforces the size limit (and keeps the daily roll), `logging::file_writer` maps `[logging]` onto it so the mapping is unit-testable, and `main` reports the applied limit instead of disclaiming it. Nine unit tests plus an end-to-end run of the compiled binary (pre-filled 1 MB day file → `.001`, 5 files with `max_files = 3` → the newest 3) back it. See §6.7 |
| A46 | `[crypto] key_rotation_days` and `[crypto] enable_pfs` were read by nothing outside the configuration itself | Two keys an operator can see described behaviour no code performed: `CryptoManager` reads only `enable_encryption` and `algorithm`, so a rotation schedule and a PFS switch were deserialised, printed by the default file generator and then dropped — the "accepted and ignored" failure A42 named for `[ui]` and A45 for `[logging]`. The default file asked for `key_rotation_days = 30`, i.e. it promised a rotation the binary never performs | **Closed by refusing what cannot be honoured**: no rotation is implementable as a local knob (the session key is the one every peer with the passphrase derives and the identity key is what peers pin, so a timer here would leave the other end unable to read), so `key_rotation_days` defaults to `0` and `AppConfig::problems` rejects anything else with that reason; the per-connection agreement has no downgrade path to select, so `enable_pfs = false` is rejected while `true` describes what the TCP transport actually does (the Tox transport carries neither identity nor ephemeral frames and leaves payload encryption to toxcore). The shipped `config.toml`, `README` and the field docs say the same thing. Covered by `config::tests::test_crypto_section_only_accepts_what_the_binary_honours` and the `crypto.key_rotation_days` / `crypto.enable_pfs` rows of `test_problems_cover_every_section` |
| A47 | `--history-limit` was parsed, defaulted (`10000`), validated — and read by no code path | The flag appeared in `--help` and the README as "number of messages kept in history" while `/history` was answered with the protocol's fixed default of 20 regardless: the number was neither a display page nor a retention bound, and the word "kept" described pruning that does not exist anywhere in the store | **Closed by giving the flag its documented meaning**: it is now the page the core answers a `Request::History { limit: None }` with (`ipc::core::CoreService::history`), the default moved to the protocol's own 20 so an invocation without the flag behaves exactly as before, and the range is the one the protocol can serve (`1..=MAX_HISTORY_LIMIT`), refused by `CliArgs::validate` rather than silently clamped. The README and `--help` no longer claim retention — the store keeps every record. Covered by `cli::tests::test_history_limit_is_bounded_by_the_protocol` and `ipc::core::tests::test_history_without_a_limit_uses_the_configured_page` (both verified to fail with the reader removed) |
| A48 | `[database] enable_migrations = false` was trusted, so a database that was never initialised opened as if it were ready | `DatabaseManager::start` skipped `run_migrations` and reported success; `is_persistent()` then returned `true` because a pool exists, so `/whoami` said the store was live while every `save_message`, `save_contact` and `/history` failed with "no such table" — one warning per write for the whole session | **Closed by checking instead of trusting**: with migrations disabled the schema is verified at startup (`require_schema`: both tables and every column this build binds) and a mismatch is one error naming what is missing and pointing at the key. A database whose schema was applied by an earlier run — the case the key exists for — still opens untouched. Covered by `database::tests::test_disabled_migrations_require_a_prepared_schema` (prepared schema accepted, schema-less file refused with the table named) |
| A49 | `/whoami`'s group count came from a cache that only `/group` refreshed | `session_info` read `self.groups`, which `/group` and transport events fill; a Tox instance that rejoined a conference after a restart (which toxcore does on its own) was not in it until something asked, so the snapshot could print `0 groups` in a session that was in one — while `/group` listed it. The transport is what `list_groups` already treats as the source of truth | **Closed**: the refresh is one method (`CoreService::refresh_group_cache`) called by both `Request::Groups` and `Request::SessionInfo`, so the snapshot and the group list cannot disagree; a transport without groups has none to report, and a group read that *fails* keeps the cache because an answer that did not arrive is not evidence that groups are gone. Covered by `ipc::core::tests::test_session_info_answers_with_the_transports_groups` (a warm cache the transport contradicts must not survive the request; verified to fail with the refresh removed) |
| A50 | A **message body** from a peer was printed without the check the send path applies | `ALLOWED_MESSAGE_CONTROLS` exists to keep `ESC` and friends out of "another user's screen" — but the check ran only on the way *out*, i.e. against our own front-end, while the direction a peer controls was unchecked: a body that decrypted was decoded as UTF-8 and handed to a front-end, which prints it. `ESC[2J` clears the screen, `ESC]52;…` writes the clipboard, a bare `\r` rewrites text the user already read — and the same body was persisted, so it came back through `/history`. A group message with an impossible body was worse: it was dropped with a `warn!` and *nothing* else, so the user lost a message without being told | **Closed at the choke point**: `core::body_of` applies the UTF-8 *and* the control-character rule once, for both transports and both conversation kinds. A refused body is reported as `CoreEvent::MessageUndecodable` with a `reason` and (for a group) the group's id and name, is counted in `/stats`, and is not stored. The event was extended rather than joined by a new tag because an unknown tag is a decode error for an older front-end (protocol v9). The transport half is pinned from below by `network::tests::test_a_message_body_reaches_the_core_verbatim`: the wire carries the escape sequence intact, so the refusal is provably the core's. Covered by `core::tests::test_a_body_with_control_characters_is_reported_not_displayed`, `test_newlines_and_tabs_still_survive_in_a_body` (the rule must not narrow what a peer can say), `test_a_group_body_with_control_characters_is_reported_with_its_group` and the updated `test_non_utf8_group_text_is_not_published` |
| A51 | Every **label** a peer chose was adopted and printed unchecked | A TCP greeting was `String::from_utf8_lossy(payload)` and stored as the peer's nickname, with no bound and no character check; on Tox the same three strings (a friend's name, a conference title, a participant's name) were lossily decoded in the snapshot readers. Those strings are printed in the peers/friends panes, in `/peers`, in the header and in log lines, they become the key of the outbox and of the pin table, and the greeting could be as large as the frame limit (64 KiB) — so a peer could clear the reader's screen, write to the clipboard, or make a 64 KiB string the key of a table that is written to disk | **Closed with one rule at every adoption point**: `ipc::validation::peer_label` (1–64 characters, no control character) is applied to the greeting and to the three Tox readers. A refused label is *not a name*: the connection, the friend entry, the frames and the counters stay, and the peer is identified by address or short key — exactly the state a peer that announced nothing is in — because repairing the string would display something the peer did not send. The greeting is a discrete event, so its refusals are counted (`MetricsView::greetings_refused`, reported by `/metrics`); a Tox label is read per snapshot, so it cannot be counted the same way, which the field's doc says. Covered by `network::tests::test_a_greeting_with_an_unusable_nickname_is_refused` (an escape sequence and a 512 byte greeting are refused, a well-formed one still works, the counter reaches 2 and not 3) and `tox::tests::test_a_conference_title_that_cannot_be_displayed_is_refused` — both verified to fail with the rule removed; the rule itself is `ipc::validation::tests::test_peer_label_accepts_only_displayable_names` |
| A52 | The shipped documents quoted a protocol version that no longer existed: the frame example in `README.md` and §3.2 of this document showed protocol version 8 and §6.4's window item said `PROTOCOL_VERSION` was 8, while the constant had been bumped to 9 by A50 — and nothing in the tree compared a document against a constant | The `README.md` example is what the document tells a foreign client to copy ("a client speaks JSON frames with a 4-byte big-endian length prefix … any other language can drive the backend"), so a reader writing a client from it saw a version this build does not announce; the same two sentences had already drifted once (7 → 8, recorded in the changelog), which is what made the missing *gate* — not the wrong digit — the finding | **Closed**: the literals state 9, and the drift now has a check instead of a proofreader — `doc_contract_test` reads `README.md` and this document from the repository and asserts, against the constants, every `protocol_version` field in a handshake example, every `` `PROTOCOL_VERSION` is N `` / `` `MIN_SUPPORTED_PROTOCOL_VERSION` is N `` prose claim, the header's window (`protocol version 9 (serves 3..=9)`), the length-prefix claim (`4-byte big-endian length prefix`), the §4 table's `Frame size` cell (`1 MiB`, from `MAX_FRAME_LEN`), the message ceiling (`32 KiB`, from `config::MAX_MESSAGE_LENGTH`) and the shipped default `app.max_message_length`, plus every option in the README's CLI table against the binary's `--help`. Verified to fail by reverting one literal to 8: the assertion names the document, the line, the value it found and the constant |


### 5.1 Measured toxcore behaviour (observed, not inferred)

Some of the dispositions above depend on what the C library actually does, so the
evidence is recorded rather than assumed. Every item below was run against
`toxcore 0.2.18`:

| Observation | How it was established |
| --- | --- |
| A conference created with `tox_conference_new` never raises `conference_connected`, even after another peer joins and both sides count 2 peers. | Probe test (3 s, zero events for a solo conference) plus the ignored DHT test: after the join, the creator reported 2 peers with `joined: false` while the joiner reported 2 peers with `joined: true`. tox.h agrees: the callback fires "after joining [a conference] with `tox_conference_join`". |
| A conference with no other participant cannot be sent to. | `tox_core_transport_test::test_group_lifecycle_over_the_tox_transport` observes the `NO_CONNECTION` refusal, and the reason is read from the conference-specific error table (the code has a different meaning per function). |
| A locally created conference counts **itself** in `tox_conference_peer_count`. | `tox::tests::test_conference_lifecycle_without_a_dht` asserts `peers == 1` right after creation, so a front-end that reads it as "other people" is off by one. |
| `tox_conference_set_title` on our own instance raises no `conference_title` callback. | Probe test: creating a conference with a title produced no events, so the rename path can only be observed from the *other* side. |
| A peer's `tox_conference_set_title` reaches us as `ConferenceTitleChanged`. | Registered callback; the DHT path is the only way to raise it, which is why the rename is exercised in the opt-in suite rather than asserted from a unit test. |
| A conference invitation cookie is stable for a (friend, conference) pair: inviting the **same** friend to the **same** conference again reuses the cookie, so the token is byte-identical after a decline. | The opt-in DHT test observed it: the second invitation carried the same 76 hex characters as the declined one (`assert_ne!` on the two tokens failed with equal values). This is why `transport::forward_event` de-duplicates an invitation it already holds (`already_pending`) — the same cookie can arrive twice — and why `/group decline` re-opens the *same* capability rather than invalidating it: the inviter can restore it, but the token itself is not fresh. A capability that must not be replayable therefore relies on the invitation being consumed by a successful join, not on the cookie being one-shot. |

## 6. Open work

1. **Split into a workspace** (`meta-text-core`, `meta-text-cli`,
   `meta-text-tui`, `meta-text-proto`) so the dependency rules of §2.1 are
   enforced by Cargo instead of review.
   **Closed**: the sketch above named four crates; turning it into a build showed
   that two more boundaries are needed for the enforcement to be real, and §2.1.1
   records all six. `meta-text-proto` (wire contract, framing, validation, shared
   error/types/helpers) is a leaf; `meta-text-backend` (crypto, database,
   `network`, `tox`, `transport`, `config`, `logging`, `cli` and `build.rs`)
   holds every subsystem; `meta-text-core` (`CoreService`/`CoreHandle`, the
   client contract and the TCP endpoint) re-exports the configuration vocabulary
   but **not** the subsystems, so a front-end that depends on it cannot name a
   key, a row or a socket; `meta-text-tui` hosts the presenter, the wording, the
   slash commands and the terminal engine (both front-ends render through them,
   so a shared crate is what keeps the graph acyclic); `meta-text-cli` is the
   REPL; the `meta-text` package is the binary plus an umbrella lib that keeps
   `meta_text::…` working for `tests/`. Two review-era violations became compile
   errors and were fixed in the same change: `config` read the endpoint's
   `DEFAULT_REQUESTS_PER_SECOND`/`DEFAULT_REQUEST_BURST` (a subsystem reaching
   into the interface layer — the constants now live in `config` and the endpoint
   re-exports them), and the protocol's key-length constant was read from `cli`
   (it is now a wire constant of `meta-text-proto`, re-exported by `cli`). The
   features moved with the code that needs them (`sqlite`/`postgres`/`mysql` to
   the SQLx crates, `tox-protocol` to the `libtoxcore` crate, `terminal-ui` to the
   crate that links `crossterm`), and CI now runs `cargo test --workspace`, since
   a bare `cargo test` would only run the binary crate's tests.
2. **Per-contact key material.** ~~Contact identifiers are still placeholders
   (`public_key` stores the identifier bytes), so confidentiality is limited to
   the shared session key.~~ **Closed** (see A12). A directed TCP message is sealed
   under key material of the pair's own, and both halves are in place:
   the **identity** is a persisted X25519 key pair per data directory, so it is
   stable across restarts, a peer pins it (`trust`) and reports a change under a
   known nickname, and a user compares fingerprints (`/whoami` for our own,
   `/peers` for a peer's) — §3.10; and the **key agreement** of §3.11 derives the
   key from a three-DH construction over the two static keys and one ephemeral per
   connection, so a passphrase holder who has seen both announced public keys can no
   longer reproduce it, and a recorded connection does not open another.
   What this item still names as open is narrower than it was: an **active
   participant on a first contact** can present its own identity and relay, because
   the identity is announced rather than signed. Closing that needs an identity
   authenticated off-channel — a signature under a key the peer already trusts, or an
   explicit "these are the fingerprints I expect" confirmation in the front-end —
   which is a product/design decision about how a user establishes trust, not a
   missing piece of the protocol. The seams are ready for it: `NetworkIdentity` holds
   the secret half, the pin already stores the public key such a check compares
   against, and `PAIR_KEY_LABEL`'s version suffix is what a signature-authenticated
   scheme would be derived under.
3. **Content types.** ~~The protocol carries UTF-8 text only; binary or
   structured payloads need a `content_type` field.~~ **Closed**:
   `ContentType::{Text, Binary}` is a first-class part of the message
   vocabulary (`Request::SendMessage`, `CoreEvent::MessageReceived`,
   `MessageView`, `messages.content_type`), carried by a dedicated TCP frame
   kind and sniffed on Tox, which has no type of its own. The body travels as
   lowercase hexadecimal through the JSON interface, so the protocol stays
   text-only while the transports carry real bytes. A *structured* payload
   (JSON, a specific media type) is still open: it needs a named media type
   rather than a two-valued flag.
4. **Compatibility window.** Keep the previous `PROTOCOL_VERSION` servable for
   one release so a front-end can be upgraded independently of the core.
   **Closed**: `MIN_SUPPORTED_PROTOCOL_VERSION` is 3 while
   `PROTOCOL_VERSION` is 9; a version 3 client is served and the rejection for
   anything else names the window. The remaining question is only *when* to
   retire a version, which is a release-policy decision.
5. **Transport hardening.** Consider a Unix domain socket with peer-credential
   checks on Unix and a named pipe on Windows. The token is now mandatory for a
   non-loopback `--ipc-listen`, so only the socket *kind* is still open.
6. **Rate limiting granularity.** The limiter is per connection; a client that
   reconnects repeatedly gets a fresh bucket. A per-peer-address budget and a
   configurable `requests_per_second` (currently only reachable through
   `ServerOptions`) would close that.
   **Closed**: the budget is keyed by the client's address
   (`ipc::server::PeerBudgets`), so a reconnect inherits the spent allowance while
   another address gets its own; the table is bounded by a ten minute idle TTL and
   by evicting the least recently seen address when it is full (a *new* address is
   therefore never refused — refusing arrivals is what would let an attacker who
   fills the table deny service to everybody else). The policy is configurable in
   `[ipc]` (`requests_per_second`, `request_burst`, `0` disables the limit) with
   `--ipc-rate` / `--ipc-burst` as per-run overrides, and the headless start logs
   the limit it is applying. Covered by
   `server::tests::test_a_budget_follows_the_address_not_the_connection`,
   `test_an_idle_address_is_forgotten`,
   `test_a_full_table_evicts_the_least_recently_seen_address`,
   `test_a_zero_rate_disables_the_limit`,
   `ipc_socket_test::test_a_reconnect_does_not_refill_the_budget` and the updated
   flood test; verified end to end against the compiled binary, where an `[ipc]`
   of one request per second shed the second request of a burst, shed a *second*
   connection from the same address and served one again 1.2 s later, while
   `--ipc-rate 0` overrode the file and served everything.
7. **Log size rotation.** `logging.rotation_size_mb` is accepted but not
   enforced, because `tracing-appender` rotates by time only; would need a
   custom appender.
   **Closed**: `logging::SizeRotatingWriter` is that custom appender — a plain
   `std::io::Write` that rolls a file over at `rotation_size_mb` *and* at the day
   boundary, names continuations `{prefix}.{date}.001`, keeps at most
   `max_files` files (active one included, oldest first), never splits a write
   across two files, writes a line larger than the whole limit whole, counts the
   bytes an earlier run left in today's file (so a restart cannot double the
   configured size) and creates a missing log directory the way the previous
   appender did. `logging::file_writer` maps the `[logging]` section onto it, so
   the configuration reaching the appender is testable — the old build could not
   assert anything about the key at all. Covered by `logging::tests` (ten tests:
   naming/ordering, the size conversion, rolling, an oversized line, retention,
   the day change, the restart, foreign files, missing directory and the
   config→writer mapping), and verified end to end against the compiled binary:
   with `rotation_size_mb = 1` and `max_files = 3`, a pre-filled 1 MB file for
   the day was rolled into `.001`, the two oldest of five files were removed, and
   the startup line reported `rolling at 1 MB, keeping 3 file(s)`.
8. **Metrics.** Expose `CoreService` counters (queue depth, drops, actor lag) on
   the protocol for operational monitoring.
   **Closed**: `/metrics` (`Request::Metrics` → `MetricsView`) reports queue
   depth/capacity, payloads queued/expired/dropped, subscribers, transport and
   traffic counters, and actor lag (`request_wait_last/max_us`,
   `request_service_last/max_us`, `requests_served`). The wait is measured from
   the front-end's enqueue timestamp, so it captures the actor being busy with a
   slow subsystem and not only a full queue.
9. **Fuzzing.** The corpus tests are deterministic and in-process; a
   `cargo-fuzz` target for `read_frame` and for the `serde` decoding of
   `ClientMessage` would add coverage-guided depth.
10. **Tox friend-request persistence.** ~~A request is held in memory until it is
    answered; accepting it on a later run needs the requester to ask again.~~
    **Closed**: an unanswered request is written to `tox-state.json` next to the
    Tox savedata and read back at startup (see §3.9), so a request the user had
    read but not answered is still there — and still answerable — after a
    restart, and answering it removes it from the file as well. The request is the
    one piece of state `toxcore` cannot keep: it is delivered as a callback and
    "not answered" *is* the refusal, so the savedata has no field for it.
11. **Tox outbox durability.** ~~Buffered payloads for an offline friend live in
    memory (bounded to 32, 5 minute TTL) and are lost on restart, exactly like the
    TCP outbox. Persisting them would need a store keyed by public key.~~
    **Closed**: the same file holds the outbox, keyed by **public key** for exactly
    the reason this item gave — a friend number is assigned from the order of the
    savedata's friend list, so it is not a durable name for a peer. `queued` now
    means "waiting for the friend", not "waiting until this process exits": the TTL
    is measured on a wall clock (a queued payload's `queued_at` became a
    `SystemTime`), a restart re-expires what is too old, drops what can no longer be
    addressed, and counts both. What the TCP outbox does remains open and is
    recorded below.
12. **Tox group/file transfer.** `tox_conference_*` and `tox_file_*` are not
    bound yet; the single `AppEvent` vocabulary would need the matching events.
    **Closed for conferences, open for files**: `tox_conference_*` is bound end to
    end — create, join, invite, send, leave, titles, peer counts, and *rename*
    (`Request::RenameGroup` / `/group rename`, with the pending-invitation list as
    `Request::GroupInvites`) — with group chat as a first-class concept in the
    protocol, the core state and the history. The `conference_title` callback is
    registered too: a peer's rename becomes `ToxEvent::ConferenceTitleChanged` →
    `AppEvent::GroupChanged`, so a front-end learns the new name from
    `group_changed` instead of waiting for the next list. A dedicated "renamed"
    wording would need a reason field on the event, i.e. another protocol change, so
    it is not claimed here. Still open: `tox_file_*` is entirely unbound (file
    transfer needs a transfer lifecycle — offer/accept/chunk/complete — not just an
    event), and a rename can only be *observed* from a second participant, so its
    peer-side assertion lives in the opt-in DHT suite rather than in a unit test.
13. **GPL-3.0 position.** Linking `libtoxcore` makes a Tox-enabled binary a
    combined work under the GPL-3.0, which is incompatible with the MIT licence
    the crate declares. The `tox-protocol` feature keeps the default build MIT;
    the licence of a Tox build is a product decision that is still open.
14. **Tox callback hand-off.** The core's inbox is bounded (backpressure), but
    the toxcore callback → bridge channel is an unbounded `std::sync::mpsc`
    because a toxcore callback cannot block without stalling the single-threaded
    instance. A peer sustaining a message rate above what the front-end renders
    would therefore grow that hand-off; bounding it needs a drop-oldest policy
    inside `tox.rs`.
    **Closed**: the hand-off is a bounded `sync_channel`
    (`tox::EVENT_QUEUE_CAPACITY`, 4096) fed with `try_send`; a shed event
    increments a counter that `/metrics` reports as `payloads_dropped`. What
    remains is the *policy* question — a shed event is currently the newest one,
    and a drop-oldest policy would need the callback to reach the receiver.
15. **The two `[network]` switches are now honest.** ~~`enable_upnp` and
    `enable_ipv6` are accepted but read by nothing.~~ **`enable_ipv6` is
    enforced**: the TCP listener binds a dual-stack IPv6 wildcard (`[::]` with
    IPv6-only cleared, through `socket2`; a host without a usable IPv6 stack falls
    back to the IPv4 wildcard) and the Tox transport passes the flag to toxcore's
    `ipv6_enabled`, so both transports honour it. **`enable_upnp` is closed by the
    other option this item offered**: no UPnP mapping client is linked, so `true`
    is rejected by `AppConfig::problems()` and the default/template ship `false` —
    the key can no longer promise a forwarded port the binary never asks for.
    A14's claim ("the whole file is validated") now holds without an exception.
16. **The Tox queue file is the transport's own store, not a second database.**
    The outbox and the request list could have gone into `database`, but §2.1
    forbids the transport from reaching it, and the SQLite driver is opt-in: a
    durable Tox queue that only worked in a `--features sqlite` build would be a
    durability guarantee that depends on an unrelated feature flag. The file
    therefore sits next to the savedata, so "the data directory" means one thing
    for the identity and for what was in flight. It carries no key material (the
    keys live in the savedata) and its bound is the transport's own, so a remote
    party cannot grow it on their schedule (R7). A failed write is reported at
    `warn` and costs durability, not delivery — the payload is still in the live
    queue.
17. **The TCP outbox is still memory only.** Its peers are addressed by
    *nickname*, which is announced per connection and is not an identity, so a
    restored queue could deliver a message to the wrong end. Two of the three things
    that blocked it are now in place: the announced identity is **stable** (§3.10, a
    persisted X25519 key pair, pinned in `net-peers.json`), and a pair key is
    **agreed** rather than derived from public values (§3.11), so a peer that
    announces a different key is a visible change and a frame sealed for the pair is
    unreadable to somebody who merely copied a public key. What remains is a
    *sealing-time* problem, and it is the reason this is a separate step rather than
    a change to the store: the core seals a payload **before** handing it to the
    transport (`CoreService` → `CoreTransport::send_to_peer(ciphertext)`), and the
    key it can use at that moment is the session key — there is no connection yet to
    agree on. A durable queue therefore holds session-sealed payloads, and an active
    participant that replays the pinned public key under that nickname (which TOFU
    reports only after the fact) could read what was queued for it, exactly as any
    passphrase holder reads a broadcast today. Making the queue sound needs the
    transport to hold the payload and seal it **when the peer becomes reachable** —
    which is what the Tox transport already does, and what the TCP path cannot do
    without moving the sealing step across the core/transport seam. Until then §3.9
    states the asymmetry plainly: TCP buffers for the session, Tox buffers across
    restarts.
18. **Runnable application cases — and the benchmarks that are still missing.** The
    project documented its API in prose and verified it in `tests/`, but had no
    `examples/` directory even though `.cursorrules` declares one, so a reader had to
    reassemble a working program from the README and the test helpers. Seven cases now
    exist (§7.1): six in the default feature set, the Tox one behind
    `required-features`, all of them compiled by `cargo test --workspace` so that the
    public API an application sees cannot drift from the one the tests use.
    **`benches/` is still open**, and it is deliberately not closed here: `criterion`
    would be a new dependency, and §4 requires a dependency to be justified by the
    path it measures. The candidates come from the bounds this document already
    states — frame parsing under the length bound (§3.1), the `PinStore` write rate
    under an identity flood (R7), and the actor's queue depth and lag under load
    (`MetricsView`) — and the rule for adding one is that a number would settle a
    design question, rather than decorate a bound that is already asserted by a test.
19. **`.cursorrules` declares three capabilities the tree does not implement.** Its
    `[specific_requirements]` block asks for file transfer, blockchain integration and
    cryptographic signatures. The first is item 12 above (`tox_file_*` is unbound), the
    third is the "announced, not signed" half of §6.2, and the second has no code at
    all: "Web3" appears only in user-facing wording (`text.rs`'s banners and the clap
    `about` line), while the identity is a locally generated X25519 key pair with no
    chain, DID or contract involved. A fourth entry, `implement_user_authentication`,
    holds in a different form: the secret is the shared `--passphrase` (and the
    endpoint's `--ipc-token`), not an account. The mismatch is recorded rather than
    resolved because each item is a product decision — what a chain would *do* here,
    which key a signature would be made with — and because the same file is a template
    that also asks for things this project deliberately does not have (a `benches/`
    directory and an 80% coverage floor, item 18).
20. **Liveness on an established TCP connection.** ~~No keepalive: an established
    idle connection is not probed.~~ **Closed**, with one documented asymmetry. TCP
    reports nothing on a read while no data arrives, so a peer that vanished without a
    FIN — a machine that lost power, a NAT that dropped its mapping — used to look
    exactly like an idle one: it stayed in the registry, `/peers` kept listing it, and
    a message addressed to it was written into a black hole until the kernel's
    retransmit timer gave up, minutes later. Two things changed:

    - **The frame.** `FRAME_KEEPALIVE` (8, §3.11) carries no payload and expects no
      reply: any received frame is equally good proof that the peer is alive, so a
      request/response handshake would add state without adding evidence. It is sent on
      a fixed cadence (20 s by default) to every greeted peer by one task for the whole
      manager, not one timer per connection.
    - **The cadence is configurable, in one direction.** `[network] keepalive_interval`
      exists for one deployment: a NAT that expires an idle mapping sooner than the
      default cadence. Probing *faster* is compatible with every peer — a shorter
      cadence only produces more evidence that this end is alive — so the value is
      accepted down to one second. Probing *slower* is rejected by
      `AppConfig::problems`, because the deadline is not derived from this value (the
      peer may be a default one probing every 20 s) and a peer enforcing the default
      deadline would drop the slower probe: the key would then be a way of breaking a
      connection that used to work, which is the mismatch the constant used to rule out.
      `0` is rejected as well, because "never probe" reopens this very hole.
    - **The deadline.** `read_loop` drops a connection that has received *nothing* for
      `IDLE_TIMEOUT` (60 s, three default cadences, so one delayed keepalive cannot tear
      down a healthy connection) and names the reason in `PeerDisconnected` ("no traffic
      within the idle timeout"), so an operator can tell a reclaimed half-open
      connection from a peer that hung up politely. It is deliberately *not* derived
      from the configured cadence: it bounds the peer's silence, not this end's probing,
      so shortening the cadence adds evidence without weakening what counts as silence.

    **The asymmetry**: the deadline arms only once the peer has *sent* a keepalive. A
    peer built before the frame kind ignores it (as it does for every late addition)
    and therefore never arms the deadline, so it keeps the old behaviour instead of
    being dropped for being quiet — half-open detection is a property of a *modern
    pair*. That is the price of not having a peer-level version negotiation, and it is
    the reason the rule is "arm on evidence" rather than "arm on greeting": a
    capability we cannot negotiate is inferred from behaviour.

    **No IPC version change was needed**, correcting what this item said before the
    work: the peer frames are the transport's own contract (§3.11), so a new kind is
    compatible by design and `PROTOCOL_VERSION` — which governs the front-end/core
    boundary — is untouched. Covered by three tests:
    `network::tests::test_a_greeted_peer_is_sent_keepalives` (the sending half, read
    off the wire), `network::tests::test_a_silent_peer_is_dropped_once_it_has_proved_it_sends_keepalives`
    (the receiving half, including the reason on the event) and
    `network::tests::test_a_peer_that_never_sends_a_keepalive_is_not_dropped_for_silence`
    (the compatibility rule). Each was verified to fail when its mechanism is removed:
    arming unconditionally, never arming, and not sending.

    **The cadence knob** is covered from the configuration side as well:
    `config::tests::test_keepalive_interval_may_be_shortened_but_not_slowed` (the whole
    accepted range, plus `0` and a slower cadence refused with the reason named),
    `config::tests::test_a_configuration_without_a_keepalive_interval_still_loads` (the
    `serde` default keeps an operator's existing file working, at the cadence the
    constant used to have),
    `network::tests::test_a_configured_keepalive_cadence_is_honoured_without_shortening_the_deadline`
    (config → wire, in a five second window a default cadence could not fill, while the
    deadline stays at three default cadences) and
    `network::tests::test_a_zero_keepalive_interval_falls_back_to_the_shipped_cadence`
    (a manager built without validation cannot end up never probing). The wire half was
    verified to fail with the loop reading the constant instead of the configured field,
    and the field half with the constructor ignoring the configuration.

    **What is still not tunable, on purpose:** `IDLE_TIMEOUT`. It is the *peer's*
    deadline, and the peer may be a default one, so deriving it from a shortened local
    cadence would drop the very peer the default keeps. That is why the knob is one-way
    rather than two-way: a two-way cadence would need the deadline to scale with it, and
    a deadline that scales with a *local* setting cannot be safe against a *remote* peer
    whose value is unknown.

21. **History retention, and key rotation, are not implemented.** Both used to be
    *implied* by configuration that did nothing: `--history-limit` read as "messages
    kept in history" (A47) and the shipped `config.toml` asked for
    `key_rotation_days = 30` (A46). The keys now say what the binary does — a display
    page, and a rotation interval that accepts only `0` — but the capabilities behind
    them are still absent, and this item exists so the two are not confused again.

    **Retention** would be a pruning rule in `DatabaseManager` (delete the oldest rows
    once a conversation, or the whole store, exceeds a bound) and it needs a policy a
    user can see before any row is removed: per conversation or globally, and what an
    operator who manages the database themselves does about it. The store today keeps
    every record the transport wrote; nothing deletes one silently, and `/history` is
    bounded per request instead (§3.8).

    **Rotation** cannot be a local timer at all. The session key is the one every peer
    holding the passphrase derives and the identity key is what peers pin, so a new key
    this end started using would be unreadable to the other end until it agreed to the
    same change — which is a handshake (a protocol change), not a schedule. What *is*
    covered is the per-connection agreement of §3.11: the pair key is new for every
    connection, so a recorded handshake does not yield the key of a later one, even
    though no long-lived key is ever rotated.

22. **`[crypto] kdf` is enforced but not shown anywhere.** The key is validated against
    `SUPPORTED_KDFS` (`Argon2` only), so a file cannot ask for a KDF the binary does not
    use and `derive_key` really is Argon2 — it is therefore *honoured*, unlike the two
    A46 closed. What is missing is the other half of what A46 established: it is not
    *visible*. `algorithm` is printed by `/version` and `/whoami` (`encryption:
    ChaCha20-Poly1305`), while the KDF is not, so an operator reading the interface cannot
    confirm which of the two configured names is in force. Closing it means giving
    `CryptoManager` the configured KDF the way it already keeps the algorithm, adding it
    to the session snapshot (an additive protocol field) and printing it beside the
    cipher. Cosmetic, but it is the same asymmetry the audit for A46 was looking for, and
    it is recorded here so that leaving it is a decision rather than an oversight.

## 7. Verifying a change

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --workspace -- -D warnings
cargo test --workspace                     # default features
cargo test --workspace --all-features      # what CI runs on three platforms
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --workspace

# End-to-end message exchange, friend-request refusal, request persistence and
# group invitation over the public Tox DHT (needs outbound UDP). The `#[ignore]`d
# tests run here, and they serialise themselves on a shared async mutex: each
# boots two full toxcore instances, and three of them in parallel starve the DHT
# round-trips they depend on, which shows up as a spurious "never saw the peer
# connect". Serialising is necessary but not sufficient: four fresh pairs
# bootstrapping from one address inside a single run can still miss the 120 s
# friend-request budget, because the DHT rate-limits a host that bootstraps
# repeatedly. That failure moves between the tests from run to run and disappears
# when one is run alone, so treat a batch failure as environmental and re-run the
# named test by itself before believing it.
cargo test -p meta-text --features tox-protocol,terminal-ui --test tox_core_transport_test -- --ignored
```

Message delivery is covered at four levels, so a regression is caught wherever
it lands:

| Level | Test | What it proves |
| --- | --- | --- |
| Transport | `integration_test::test_peers_exchange_messages_end_to_end` | Two `NetworkManager`s exchange an encrypted frame. |
| Core (in-process) | `message_exchange_test` | Two actors: attribution, order, byte-exact bodies, both directions, offline buffering then flush. |
| Binary + network | `repl_commands_test::test_two_run_instances_exchange_messages` | The compiled binary, a real REPL, two processes. The client is kept resident and only addresses the peer once it has seen it connect, the server's printed frame is awaited, and the client then quits and must exit cleanly — a one-shot script (`/msg` immediately followed by `/quit`) raced the handshake and dropped a message buffered for a peer that was still offline to the client. |
| Tox, real DHT (opt-in) | `tox_core_transport_test::test_two_cores_exchange_a_message_over_tox` | Friend request → accept → buffered message flushed → live message, over the public network. |

The core-level tests wait for the peer's *announced nickname* before sending, not
for `connected_peers > 0`: on TCP the socket is counted one `hello` frame before
the peer becomes addressable, and a send inside that window is buffered and
flushed rather than reported as `sent` (see A31).

The Tox queue is covered at two levels. Without a network: `tox_store::tests`
(layout, atomic replace, corrupt and unknown-version files, bounds) and
`transport::tests::test_the_queue_round_trips_through_the_store`,
`test_a_payload_that_expired_while_down_is_not_restored`,
`test_a_payload_that_cannot_be_addressed_is_dropped`,
`test_a_restored_queue_is_capped_keeping_the_delivery_order`,
`test_restored_requests_are_bounded_and_deduplicated` and
`test_a_timestamp_in_the_future_is_treated_as_just_queued` (the translation from a
stored public key back into a friend number, which is the part a restart can get
wrong), plus `transport::tests::test_a_queued_payload_survives_a_restart`, which
restarts a real `ToxTransport` over the same data directory (both instances are
local, so no DHT is involved). End to end:
`tox_core_transport_test::test_a_pending_friend_request_survives_a_restart_over_tox`
is the only test that can prove the *request* half, because a pending request
exists only as a callback from a second instance over the real DHT.

Delivery of a *restored* payload once the peer appears is deliberately the
composition of two tests rather than one longer one:
`test_a_queued_payload_survives_a_restart` proves the payload is back in the live
outbox, and `tox_core_transport_test::test_two_cores_exchange_a_message_over_tox`
proves a live outbox entry is flushed when the friend connects. A third DHT test
that did both would add a minute of waiting to the suite to re-assert what each
half already pins.

The identity, the pins it feeds (§3.10) and the key agreement they authenticate
(§3.11) are covered at four levels, because each level can fail on its own:

| Level | Test | What it proves |
| --- | --- | --- |
| Key pair, agreement and file | `identity::tests` (17) | The announced value is the public half of the stored secret, the fingerprint is a function of the key in eight groups, a damaged secret or a future layout is refused rather than reused, a restart over the same directory yields the same identity, the file is owner-only on Unix, and `Debug` does not print the secret. For the agreement: both ends derive the same key, each connection derives a *different* one, the result is **not** what an observer of the handshake computes, a substituted identity does not yield the key the honest pair has, and the ephemeral's `Debug` is redacted too. |
| Pin table | `trust::tests` (14) | First sighting, unchanged sighting and *change* are distinguished, a change is counted and flagged per peer while the pin moves on, a pin outlives a restart (and a change across it is still a change), the table is bounded with the excess counted, a flood of changes cannot rewrite the file (the write rate is a function of the table, not of the frame rate), nicknames match case-insensitively, the file is written in a stable order, and a volatile store still detects a change. |
| Transport + core wiring | `network::tests::test_a_peer_identity_is_pinned_and_a_change_is_reported`, `core_service_test::test_the_identity_survives_a_restart` | The transport actually feeds the store from `FRAME_IDENTITY` (a pin nothing reports would look identical to a working one), and two cores over one data directory announce the same identity and the same fingerprint — i.e. the core uses the store instead of a per-session random value. |
| Directed traffic | `network::tests::test_a_directed_message_uses_the_pair_key`, `test_the_pair_key_ladder`, `test_the_three_step_ladder_on_a_connection`, `test_a_repeated_ephemeral_does_not_re_key_the_connection` | A real connection agrees a key and a directed message crosses under it; the key the two ends hold is *not* the derivation an observer can compute, and a third pair gets one of its own; the ladder is what the documentation says (agreed key preferred, static derivation for a peer without an ephemeral or with an unusable one, the session key when the pair has no key) and a receiver opens a frame sealed with **any** of the three, which is the compatibility path a peer from before the agreement depends on; and an ephemeral is taken once per connection, so a repeated or tampered frame cannot re-key it. |

Group chat is covered without a DHT by `tox::tests::test_conference_lifecycle_without_a_dht`
and `tox_core_transport_test::test_group_lifecycle_over_the_tox_transport` (create,
list, send, unknown-group refusal, leave), which also pins `joined` to the
handshake rather than to "listed by toxcore" (A34).

The paths that need a second participant run in the opt-in DHT suite:
`test_two_cores_exchange_a_group_invitation_over_tox` (invite → read the token from
`Request::GroupInvites` → join → the spent token is refused → group message
received → rename, which the *other* peer observes as `group_changed`), the
friend-request *yes* path (`test_two_cores_exchange_a_message_over_tox`) and its
*no* path (`test_two_cores_exchange_and_refuse_a_friend_request_over_tox`). The
rename is the clearest example of why that suite matters: toxcore raises
`conference_title` only for a peer's rename, so our own change can never be
observed from a unit test. A report of "this is untested" is therefore only true
of the default run, not of the suite as a whole: run the `--ignored` line above
before claiming either path is broken.

Both feature builds matter and both are run: `cargo test` (no features) must be
green on its own, so anything that needs the SQL driver — the history read-back in
`test_group_message_is_published_counted_and_persisted`, the whole of
`database::tests::test_group_message_round_trips_through_the_database`, and the two
tests that pin the store's startup contract
(`database::tests::test_disabled_migrations_require_a_prepared_schema`, which needs a
driver to have a schema to check, and
`ipc::core::tests::test_history_without_a_limit_uses_the_configured_page`, which needs
records to page through) — is behind `sqlite`, while the event, counter and state
assertions run in every build. A test
that needs a driver but is not gated turns the default build red for a reason that
has nothing to do with the change being tested.

The configuration and command line contracts are covered in every build:
`config::tests::test_crypto_section_only_accepts_what_the_binary_honours` and
`test_problems_cover_every_section` hold `[crypto]` to the values the binary keeps,
`cli::tests::test_history_limit_is_bounded_by_the_protocol` holds `--history-limit` to
the page the protocol can answer, and `ipc::core::tests::test_session_info_answers_with_the_transports_groups`
pins `/whoami` to the transport's group list rather than to the session's cache.

The end-to-end suite spawns the compiled binary and scripts the REPL over stdin
(`tests/repl_commands_test.rs`), which is what makes the presentation port
verifiable: any change in wording or dispatch order fails the build. A client that
has to stay resident across a handshake (or be waited on) is driven through
`RunningInstance`, which mirrors stdout as it is written and feeds input line by
line; its reader runs on a plain OS thread rather than `tokio::io::stdin()`, because
the latter's blocking read is not cancellable and would keep the process alive after
`/quit` while stdin is open — the pitfall a pty reproduction turned up, and the
reason the test can now assert the client's own exit code.
`test_tui_fallback_quits_with_stdin_open` does the same for the TUI's fallback
reader, which shares the shape but not the code path.

### 7.1 Runnable application cases (`examples/`)

This document describes a *contract*; [`examples/`](../examples) holds the ways that
contract is meant to be used, one file per case, so a reader can run the case instead
of reconstructing it from a test:

| Case | Example | Needs |
| --- | --- | --- |
| Embed the headless core: requests, refusals, events, clean shutdown | `embed_core` | — |
| Attach the line oriented REPL to an embedded core (scriptable over a pipe) | `repl_session` | stdin |
| Attach the full screen interface | `tui_session` | a terminal for the alternate screen; `--features terminal-ui` for the real engine |
| Two instances exchanging an encrypted message over TCP | `peer_chat` | loopback |
| A front-end in another process (`RemoteClient`) | `remote_frontend` | a `--mode core --ipc-listen` instance |
| The announced identity, the pair secret and the pin table (A12) | `pin_identity` | — |
| The Tox transport directly | `tox_transport` | `--features tox-protocol` and libtoxcore |

`cargo test --workspace` builds every example — the Tox one is skipped without its
feature through `required-features` — and `cargo clippy --all-targets` lints them, so
an example that stops compiling or stops matching the public API fails the default CI
run rather than a reader. They are held to the test suite's hermeticity rule as well:
a temporary data directory, loopback addresses, and nothing written into the working
tree.

The out-of-process endpoint is covered by `ipc_socket_test`, which drives a real
socket: the handshake (a wrong or missing token is refused, and a remote client cannot
stop the core), version negotiation, the rate limiter (a flood is shed, and
reconnecting does not refill the address's budget), a silent client being dropped by
the handshake timeout, and — added with the client audit —
`test_a_client_with_a_dead_core_fails_promptly`: a session the core has closed is
reported as a **network** error at once, not after the 30 s deadline, which is the
difference between a front-end that notices a stopped core and one that appears to
hang for half a minute per request.



