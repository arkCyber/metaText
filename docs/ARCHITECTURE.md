# metaText architecture and interface control document

This document is the design authority for the split between the metaText user
interfaces (CLI, TUI) and the core application service. It records the interface
contract, the design controls applied, the audit that motivated the refactor and
the findings that are still open.

Version: 0.4.0 — protocol version 2.

Protocol history:

| Version | Change |
| --- | --- |
| 1 | Initial request/reply/event vocabulary. |
| 2 | Added `CoreEvent::MessageUndecodable`; undecodable payloads are reported instead of being replaced with U+FFFD. |

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
        ui/cli  (REPL)          ui/tui  (full screen)
              \                        /
               \   ipc protocol (ICD) /
          +-----------------------------------+
          | ipc::client  /  ipc::server       |   transports
          +-----------------------------------+
                          |
          +-----------------------------------+
          | ipc::core  CoreService + CoreHandle|   backend (actor)
          +-----------------------------------+
             |            |            |
          crypto       database     network
```

### 2.1 Layers and allowed dependencies

| Layer | Modules | May depend on |
| --- | --- | --- |
| Presentation | `ui::cli`, `ui::tui`, `ui::presenter`, `ui::text`, `tui` | `ipc::client`, `commands`, `error` |
| Interface | `ipc::protocol`, `ipc::framing`, `ipc::validation`, `ipc::client`, `ipc::server` | `error`, `utils` |
| Backend | `ipc::core` | `config`, `crypto`, `database`, `network`, `types`, `cli` |
| Subsystems | `crypto`, `database`, `network`, `config` | `error`, `types`, `utils` |

Dependency direction is strictly downward. In particular:

- The presentation layer never names `crypto`, `database` or `network`. This is
  the mechanical expression of R3: a UI cannot construct a key, write a row or
  open a socket.
- The backend never formats user-visible text; every string lives in
  `ui::presenter` / `ui::text`, so wording can change without touching the core
  and the core stays testable without a terminal.
- `tui` is a rendering engine only. It reports `TuiInput::Line` / `TuiInput::Quit`
  and renders `TuiInfo` plus the output sink; it has no knowledge of `AppEvent`.

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
{"type":"hello","protocol_version":2,"token":"…","client":"tui/0.4.0"}
```

The server answers `Welcome` (carrying a `SessionInfo` snapshot) or `Rejected`
with a typed error, then closes. The handshake is bounded by a timeout, and a
client that does not greet in time is dropped without affecting other clients.

Rejection causes:

| Cause | `ErrorCode` |
| --- | --- |
| First frame is not `Hello` | `invalid_request` |
| `protocol_version` mismatch | `unsupported_protocol` |
| Token missing or wrong | `unauthorized` |

The token comparison folds the length difference into the result instead of
returning early, so it does not leak the secret through timing.

### 3.3 Requests

Internally tagged (`{"type": …, …}`) so an unknown kind fails fast instead of
being misread. One variant per user-visible operation:

`ping`, `session_info`, `statistics`, `list_contacts`, `add_contact`,
`remove_contact`, `set_nickname`, `set_status`, `select_conversation`, `connect`,
`send_message`, `history`, `save_session`, `shutdown`.

**Ordering guarantee.** Requests arriving on one connection are executed
strictly in arrival order, and their replies are emitted in that same order.
A client that sends `add_contact` followed by `list_contacts` therefore always
observes the addition. The server keeps this guarantee without blocking event
delivery by queueing requests onto a single dispatcher task per connection
(bounded queue, §3.7).

### 3.4 Replies

`Reply` carries data only, never formatted text: `pong`, `session`,
`statistics`, `contacts`, `contact_added`, `contact_exists`, `contact_removed`,
`conversation`, `saved`, `updated`, `sent`, `shutting_down`.

### 3.5 Events

`CoreEvent` is broadcast, not queued per client, so a slow consumer cannot stall
the core: `ready`, `message_received`, `message_undecodable`,
`message_delivered`, `peer_connected`, `peer_disconnected`,
`conversation_changed`, `nickname_changed`, `status_changed`, `shutdown`. A
lagging socket subscriber is reported server-side and skipped, never buffered
without bound.

`message_undecodable` is the honest counterpart of `message_received`: a frame
that decrypted successfully but is not valid UTF-8 is reported with its size
instead of being rendered as replacement characters.

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
| Request deadline (remote) | 30 s | `ipc::client` |
| Message length | `app.max_message_length` (1372), hard ceiling 32 KiB | `ipc::core`, `config::MAX_MESSAGE_LENGTH` |
| Friend list | `app.max_friends` (1024) | `ipc::core` |

Bounding the transport → core queue means a peer that sends frames faster than
the core can absorb them is *throttled* (the transport awaits the send) rather
than growing an unbounded queue. Nothing is dropped, so no message is lost
under load.


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
| R11 (bounded memory) | Bounded transport queue with backpressure; bounded per-client queues | `core_service_test::test_command_queue_applies_backpressure`, `network::EVENT_INBOX_CAPACITY` |
| R12 (config integrity) | `AppConfig::problems()` rejects an unusable file before any socket is bound; configured defaults re-validated inside the library | `config::tests::test_problems_cover_every_section`, `core::tests::test_invalid_configured_defaults_are_not_applied` |
| Change control | `PROTOCOL_VERSION` negotiated per connection | `ipc_socket_test::test_protocol_version_is_announced`, `test_stale_protocol_version_is_refused` |
| Robustness | Deterministic corpus tests: arbitrary frames round-trip, arbitrary garbage is handled, one-byte-at-a-time delivery reassembles | `ipc::framing` tests |
| Availability | A hostile client cannot occupy an endpoint slot forever or break it for others | `ipc_socket_test::test_silent_client_is_dropped_by_the_handshake_timeout`, `test_oversized_frame_does_not_break_the_endpoint` |

Additional invariants enforced by construction:

- **No `unsafe`** (`#![deny(unsafe_code)]`) and **no undocumented public item**
  (`#![deny(missing_docs)]`).
- **No panics on the interface path**: malformed input becomes `ErrorInfo` or an
  I/O error; the actor never unwraps a reply channel.
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
| A12 | Contact identifiers are placeholders, not real keys | No per-contact confidentiality | **Open** (pre-existing, unrelated to the split) |
| A13 | Crate is still a single Cargo package with internal module boundaries | The boundary is enforced by convention and review, not the compiler | **Open**: see §6 |
| A14 | Configuration was read but not validated as a whole, and several values were ignored (`connection_timeout`, the whole `[logging]` section, bootstrap addresses) | A typo in the file surfaced late or silently; an operator could not actually change the log destination | **Closed**: `AppConfig::problems()` covers every section, `main` fails fast, and the transport/logging now honour their configuration |
| A15 | User input was only length-checked for messages | A nickname or identifier could be unbounded, contain NUL, or carry terminal escape sequences into another user's screen | **Closed**: `ipc::validation` enforces per-field rules at the boundary |

## 6. Open work

1. **Split into a workspace** (`meta-text-core`, `meta-text-cli`,
   `meta-text-tui`, `meta-text-proto`) so the dependency rules of §2.1 are
   enforced by Cargo instead of review.
2. **Per-contact key material.** Contact identifiers are still placeholders
   (`public_key` stores the identifier bytes), so confidentiality is limited to
   the shared session key.
3. **Content types.** The protocol carries UTF-8 text only; binary or structured
   payloads need a `content_type` field.
4. **Compatibility window.** Keep the previous `PROTOCOL_VERSION` servable for
   one release so a front-end can be upgraded independently of the core.
5. **Transport hardening.** Consider a Unix domain socket with peer-credential
   checks on Unix and a named pipe on Windows, and make the token mandatory for
   non-loopback binds.
6. **Rate limiting.** There is no per-client request rate limit yet; the bounded
   queues bound memory, but not CPU spent on rejected work.
7. **Log size rotation.** `logging.rotation_size_mb` is accepted but not
   enforced, because `tracing-appender` rotates by time only; would need a
   custom appender.
8. **Metrics.** Expose `CoreService` counters (queue depth, drops, actor lag) on
   the protocol for operational monitoring.
9. **Fuzzing.** The corpus tests are deterministic and in-process; a
   `cargo-fuzz` target for `read_frame` and for the `serde` decoding of
   `ClientMessage` would add coverage-guided depth.

## 7. Verifying a change

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --workspace
cargo test                     # default features
cargo test --all-features      # what CI runs on three platforms
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
```

The end-to-end suite spawns the compiled binary and scripts the REPL over stdin
(`tests/repl_commands_test.rs`), which is what makes the presentation port
verifiable: any change in wording or dispatch order fails the build.



