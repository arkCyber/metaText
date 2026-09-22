# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Documentation

- **The workspace split left two documents - and the crate READMEs - describing a
  tree that no longer exists, and one of them quoted a protocol version the build
  does not speak.** `crates/meta-text-proto/README.md` still said
  `` `PROTOCOL_VERSION` 8 `` and `serving 3..=8` while the constant has been 9 since
  the version 9 wire change; that file is the proto crate's crates.io landing page, so
  a reader who depends on the wire layer alone was handed a compatibility window this
  build does not serve. It was never in `doc_contract_test`'s scope - the test read
  `README.md` and `docs/ARCHITECTURE.md` only - which is exactly the shape of A52 one
  level down: the drift was caught by hand, not by the gate that exists to catch it.
  The README now states the version *and* the two framing bounds (`FRAME_HEADER_LEN`
  4 bytes, `MAX_FRAME_LEN` 1 MiB), the same way the two shipped documents do. The
  drift now has a gate rather than a proofreader:
  `doc_contract_test::test_the_crate_readmes_state_the_current_protocol_version`
  reads all five member READMEs, checks the version claim of every one that makes it
  (a crate that cannot reach the wire contract, such as `meta-text-cli`, is skipped
  rather than required to invent a claim), and refuses to pass vacuously if no crate
  README states the window any more.
- **The architecture sections described the pre-workspace module tree.** `README.md`
  §Architecture and `docs/ARCHITECTURE.md` §2 drew and tabulated `ui/cli`, `ui/tui`,
  `ui::presenter` and `ui::cli` - paths that stopped existing when the layers became
  crates - so a reader following a rule to its module landed nowhere. Both now name
  the crate that owns each module (`meta-text-cli`'s `ui`, `meta-text-tui`'s
  `presenter` / `text` / `tui` / `ui::tui`, and so on), the §2.1 layer table carries
  the owning crate per row, and the README's module table gained the `identity`,
  `trust`, `transport` and `ipc::validation` rows it was missing. The three rustdoc
  layer diagrams that repeated the same two names (`meta-text-proto::ipc`,
  `meta-text-core`, `meta-text-core::ipc`) were corrected with them.
- **The README's workspace table now links each crate's README**, so the layout table
  is also the way to reach the page that documents a single layer, next to
  `docs/ARCHITECTURE.md` (the contract) and `CHANGELOG.md` (the release history).

### Added

- **`PageUp` / `PageDown` now move a page, and the side panes say what they cannot
  show.** The key loop moved the offset by five *log lines* and clamped it against the
  log's line count, which stopped matching what the pane draws as soon as a line
  wrapped: with a long answer on screen (`/metrics` is 35 lines and most of them wrap)
  one key press could move a screenful or nothing at all. The pane now publishes what a
  page is - its own height in rows - and how far back the log goes, so one press is one
  page in rows, and a log shorter than a page stops at its first row instead of
  scrolling into blank space. The bound is only known once the walk back reaches the
  start of the log, which is why it is reported as `usize::MAX` until then: the value
  can only ever fail to clamp, never block a key press. The friends and peers panes
  clip a list taller than their box, which looks exactly like a short list - the friend
  list alone holds up to 1024 entries - so their titles now carry `· +N more`. Covered
  by `tui::tests::test_visible_log_rows_scrolls_by_rows`,
  `tui::tests::test_visible_log_rows_scrolls_back` (which asserts the reported bound)
  and `tui::tests::test_title_with_overflow`, plus the terminal suite's checks for the
  scroll mark and for the marker on a deliberately short terminal.
- **The interface's input row now scrolls a draft that is wider than the pane, and
  places the caret by display column.** A chat draft is far wider than a terminal
  (the limit is 1372 bytes), and the row rendered the whole draft with a widget that
  truncates: everything past the pane's width was invisible while it was being typed,
  and the caret — a *character* offset — sat one column away from the truth for every
  emoji or CJK character before it. `input_view` keeps the caret inside the row by
  dropping leading characters only as far as the caret requires, marks the hidden text
  with a leading `…`, and measures both the window and the caret with the same
  `unicode-width` columns the widget uses, so the caret lands where the next character
  will go. Bracketed paste is now one line by construction as well: line breaks and
  tabs become spaces and other control characters are dropped, because a draft
  containing a newline was drawn as its first line only and could not be edited.
  Covered by `tui::tests::test_input_view_keeps_the_caret_visible`,
  `tui::tests::test_input_view_counts_columns_not_characters` and
  `tui::tests::test_paste_stays_on_one_line`.
- **`unicode-width` is now a direct dependency of the presentation layer.** It was
  already in the tree as a dependency of `tui`, and the pane and the input row need
  its measure to agree with what the widget draws; declaring it (0.1, behind
  `terminal-ui`) keeps a wrap from being applied twice with two different rules.

- **The liveness probe cadence is now configurable — downwards only.**
  `[network] keepalive_interval` (1–20 s, default 20 s) sets how often an empty
  `FRAME_KEEPALIVE` is sent to each greeted peer. It closes the one case §6.20 left
  open: a NAT that expires an idle mapping sooner than the default cadence can now be
  kept alive by probing more often. The value is **one-way** because the idle deadline
  is not derived from it: this end's deadline bounds the *peer's* silence, and the peer
  may be a default one probing every 20 s, so a slower cadence would be dropped for
  silence — a tuning knob that breaks connections which used to work, which is the
  mismatch the transport constant existed to prevent. `problems()` therefore rejects
  anything above the default (with the reason in the message) and `0`
  ("never probe" would reopen the half-open hole), and accepts every value from one
  second up to the default. `IDLE_TIMEOUT` stays 60 s for every accepted value: a
  shorter cadence only ever adds evidence, it never weakens what counts as silence.
  `NetworkManager::new` falls back to the shipped cadence when handed `0` by a caller
  that did not validate, because it is public and every integration test builds a
  configuration in memory. The field carries a `serde` default, so a `config.toml`
  written while the cadence was a constant still loads — at exactly the cadence it had.
  Covered by `config::tests::test_keepalive_interval_may_be_shortened_but_not_slowed`,
  `config::tests::test_a_configuration_without_a_keepalive_interval_still_loads`,
  `network::tests::test_a_configured_keepalive_cadence_is_honoured_without_shortening_the_deadline`
  (the frame is read off the wire inside a window a default cadence could not fill, and
  the deadline is asserted unchanged) and
  `network::tests::test_a_zero_keepalive_interval_falls_back_to_the_shipped_cadence`.
  Each half of the mechanism was verified to fail when removed: ignoring the configured
  value in the constructor fails the field assertion, and reading the constant in the
  keepalive loop fails the wire assertion. Documented in `README.md` (networking model,
  configuration, known limitations), `config.toml` and `docs/ARCHITECTURE.md` §3.11,
  §6.20.

### Fixed

- **A message body from a peer is no longer printed without the check the send path
  applies.** `ALLOWED_MESSAGE_CONTROLS` exists to keep `ESC` and friends out of "another
  user's screen", but the rule ran only on the way *out*: a body that decrypted was
  decoded as UTF-8 and handed to a front-end, which prints it. A peer could therefore put
  `ESC[2J` (clear the screen), `ESC]52;…` (write the clipboard) or a bare `\r` (rewrite
  text already read) into a conversation — and the same body was stored, so it came back
  through `/history`. A *group* body with an impossible payload was worse: it was dropped
  with a `warn!` and nothing else, so a message could vanish without the user being told.
  The rule is now applied where a body becomes text (`core::body_of`), once for both
  transports and both conversation kinds: a refused body is reported as
  `CoreEvent::MessageUndecodable` with the **reason** (`is not valid UTF-8 text` /
  `contains control characters`) and, for a group message, the group's id and name — it is
  counted in `/stats` and *not* stored. An existing event was extended instead of a new tag
  added, because an unknown tag is a decode error for every front-end built before it
  (protocol v9). `\n` and `\t` stay allowed, so a multi-line message is unaffected.
  Covered by `core::tests::test_a_body_with_control_characters_is_reported_not_displayed`,
  `test_newlines_and_tabs_still_survive_in_a_body`,
  `test_a_group_body_with_control_characters_is_reported_with_its_group` and the updated
  `test_non_utf8_group_text_is_not_published`, plus
  `network::tests::test_a_message_body_reaches_the_core_verbatim`, which pins the boundary
  from below (the wire carries the escape sequence intact, so the refusal is the core's and
  not the transport's).

- **A label a peer chose is bound and checked before it is displayed.** A TCP greeting was
  `String::from_utf8_lossy(payload)` stored as the peer's nickname — no length bound, no
  character check — and on Tox the same three strings (a friend's announced name, a
  conference title, a participant's name) were lossily decoded in the snapshot readers.
  Those strings are printed in the peers and friends panes, in `/peers`, in the header and
  in log lines, they become the key of the outbox and of the pin table, and the greeting
  could be as large as the frame limit (64 KiB). One rule now applies at every adoption
  point — `ipc::validation::peer_label` (1–64 characters, no control character) — and a
  refused label is *not a name*: the connection, the friend entry, the frames and the
  counters stay, and the peer is identified by address or short key, exactly like a peer
  that announced nothing. Nothing is repaired, because a sanitised name would display
  something the peer did not send. A refused TCP greeting is also counted
  (`/metrics`: `greetings_refused`), since it is a discrete event a hostile peer can repeat
  per connection. Covered by `network::tests::test_a_greeting_with_an_unusable_nickname_is_refused`,
  `tox::tests::test_a_conference_title_that_cannot_be_displayed_is_refused` and
  `ipc::validation::tests::test_peer_label_accepts_only_displayable_names`; both transport
  tests were verified to fail with the rule removed. Documented in `README.md`, and the
  protocol bump (v9) plus the rule itself in `docs/ARCHITECTURE.md` §3.7, §3.8, §4 (R35)
  and §5 (A50, A51).

- **`[crypto] key_rotation_days` and `[crypto] enable_pfs` are no longer accepted and
  ignored.** `CryptoManager` reads `enable_encryption` and `algorithm` and nothing else,
  so a rotation schedule and a PFS switch were deserialised, printed by the default file
  generator and then dropped — and the shipped `config.toml` asked for
  `key_rotation_days = 30`, i.e. it promised a rotation the binary never performs. No
  rotation is implementable as a local knob (the session key is the one every peer
  holding the passphrase derives, and the identity key is what peers pin, so a timer here
  would leave the other end unable to read what this one writes), so the key now defaults
  to `0` and `AppConfig::problems` refuses anything else with that reason; the
  per-connection agreement has no downgrade path to select, so `enable_pfs = false` is
  refused (with `true` describing what the TCP transport actually does — the Tox
  transport carries neither identity nor ephemeral frames and leaves payload encryption
  to toxcore). The default stays loadable, so an existing `config.toml` written from the
  reference file still starts. Covered by
  `config::tests::test_crypto_section_only_accepts_what_the_binary_honours` and the
  `crypto.key_rotation_days` / `crypto.enable_pfs` rows of
  `config::tests::test_problems_cover_every_section`; documented in `README.md`
  (configuration, known limitations), `config.toml` and `docs/ARCHITECTURE.md` §5 (A46).

- **`--history-limit` now decides what `/history` shows.** The flag was parsed, defaulted
  to `10000`, validated against zero — and read by no code path, so `/history` was always
  answered with the protocol's fixed default of 20 while the help text and the README
  described a retention bound ("number of messages kept in history") that the store never
  applied. It is now the page a `Request::History { limit: None }` is answered with, the
  default moved to the protocol's own `20` (so an invocation without the flag behaves
  exactly as before) and the range is the one the protocol can serve, `1..=MAX_HISTORY_LIMIT`
  (`ipc::protocol::DEFAULT_HISTORY_LIMIT` / `MAX_HISTORY_LIMIT`): `CliArgs::validate`
  refuses anything outside it before a socket is bound instead of letting the core clamp
  it silently. `/history 50` still wins over the flag. Covered by
  `cli::tests::test_history_limit_is_bounded_by_the_protocol` and
  `ipc::core::tests::test_history_without_a_limit_uses_the_configured_page` — both
  verified to fail with the reader removed (`unwrap_or(20)` in the core fails the first
  assertion, `10000` in `CliArgs::default` fails the flag test). Documented in `README.md`
  (`--history-limit`, `/history`, known limitations) and `docs/ARCHITECTURE.md` §3.8, §5
  (A47); `--history-limit 10000` is now a startup error rather than a silently different
  number.

- **`[database] enable_migrations = false` is checked instead of trusted.** With the key
  off, `DatabaseManager::start` skipped the schema and reported success, after which
  `is_persistent()` returned `true` (a pool exists) while every write failed with "no such
  table" — one warning per message for the rest of the session, and `/history` reporting
  an error in a session that looked configured. The store is now verified at startup:
  both tables and every column this build binds are looked up in `sqlite_master` /
  `PRAGMA table_info`, and a mismatch is a single error naming what is missing and
  pointing at the key — while a database whose schema was applied by an earlier run (the
  case the key exists for) still opens untouched. Covered by
  `database::tests::test_disabled_migrations_require_a_prepared_schema`; documented in
  `README.md`, `config.toml` and `docs/ARCHITECTURE.md` §5 (A48).

- **`/whoami` no longer answers its group count from a stale cache.** The snapshot read
  the `self.groups` cache that only `/group` (and transport events) fill, so a Tox
  instance that rejoined a conference after a restart — which toxcore does on its own —
  printed `0 groups` on `/whoami` while `/group` listed it. `CoreService::refresh_group_cache`
  is now a single method called by both `Request::Groups` and `Request::SessionInfo`, so
  the two cannot disagree; a transport without groups (TCP) has none to report, and a
  group read that fails keeps the cache because an answer that did not arrive is not
  evidence that the groups are gone. Covered by
  `ipc::core::tests::test_session_info_answers_with_the_transports_groups`, verified to
  fail with the refresh removed; documented in `README.md` (known limitations).

- **The `[ui]` section is no longer accepted and ignored.** Every key under `[ui]`
  was read by nothing outside the configuration itself: `theme`, `enable_colors`,
  `enable_mouse` and `auto_scroll` were documented, validated and then dropped on the
  floor, and `message_format` was a template no code ever looked at — the exact
  "accepted and ignored" failure `network.enable_upnp` is refused for. The three
  switches and the theme are now implemented, and the one that cannot be is refused.
  The presentation layer is written against the core protocol and cannot read the
  configuration, so the composition root translates the section into a new
  `meta_text_tui::tui::UiOptions`, which the full screen interface keeps:
  `enable_colors = false` makes every style the terminal's own (a new `Palette` owns
  the decisions, so no colour is left behind at a call site), `theme = "light"` swaps
  the palette for one that stays readable on a bright background, `enable_mouse`
  captures the wheel and scrolls the conversation pane with it (the capture is
  released again on exit), and `auto_scroll = false` keeps the pane on the rows it is
  showing while output arrives — the offset grows by exactly the display rows that
  arrived, which is measured in rows rather than lines because a wrapped line is
  more than one, and the first frame counts nothing because the startup banner did
  not arrive *while the user was reading*. `TuiFrontend::start` therefore takes the
  options, `TuiManager::new` keeps them, and the header now shows the configured
  `[app] name`, which used to be passed into a parameter the manager ignored.
  `ui.theme` is validated against the themes the interface can draw and
  `ui.message_format` against the one value it renders, both refused with a reason
  (the shipped `config.toml` stays loadable). Covered by
  `tui::tests::test_palette_follows_the_options`,
  `tui::tests::test_pane_scroll_step_is_a_page_clamped_to_the_bound`,
  `tui::tests::test_arrivals_are_counted_in_rows`,
  `config::tests::test_ui_section_only_accepts_what_the_interface_honours` and the new
  `scripts/tui-e2e/phase4_ui_options.py`, which runs the real binary against generated
  configurations and asks the terminal what changed: a captured mouse, a frame with
  no colour sequence at all, a wheel that scrolls, and a pane that does not follow
  output until `PgDn`.

- **The conversation pane no longer swallows the answer that follows a wrapping
  line.** `draw_log` took a window of `height - 2` *log lines* and handed it to the
  widget with `Wrap` enabled, so one line that wrapped made the paragraph taller than
  the pane; a paragraph is drawn from its top, so the newest rows — the reply the user
  had just asked for — fell outside it and stayed invisible. It was reachable with a
  full pane: `/metrics`, a pair of `/connect` errors and `/group create alpha` were
  enough, after which the next `/nick` *was* applied (the header showed the new
  nickname) while its confirmation never appeared. The window is now measured in
  *display rows*: `visible_log_rows` wraps each line with the same `unicode-width`
  columns the widget uses, keeps at most `height - 2` rows, and the pane renders them
  without wrapping again, so nothing reflows. Scrolling past the start of the log now
  shows the first line instead of an empty pane, because the offset outlives the lines
  it counted. Covered by `tui::tests::test_visible_log_rows_keeps_the_newest_row`,
  `tui::tests::test_visible_log_rows_scrolls_back`,
  `tui::tests::test_wrap_line_fits_the_pane` and
  `tui::tests::test_wrap_line_counts_display_columns`; the end-to-end case (a pane
  filled by `/metrics`, two `/connect` errors, `/group create alpha` and `/list`,
  then a `/nick` that must still be drawn) is what the terminal suite in
  `scripts/tui-e2e` asserts.
- **`/history` no longer credits the peer with the reader's own action.** An outgoing
  third-person action was rendered with `message.peer` as its actor, so a line the user
  had sent as `* Alice waves` came back from `/history` as `→ Bob: * Bob waves` — the
  opposite of what happened. `history_body` now names the local nickname for a message
  this instance sent and the peer for one it received, and keeps text and binary bodies
  as they were. Covered by `presenter::tests::test_history_action_names_its_actor` and
  `presenter::tests::test_history_body_keeps_text_and_binary`.
- **`/add` can no longer panic the core with a multi-byte identifier.** A Tox address
  is `key (32 B) || nospam || checksum`, and the key was taken with
  `identifier[..64]` — 64 *bytes* — while the identifier is only checked for being
  non-empty, at most 128 characters, whitespace free and control free. Any value of
  64 bytes or more whose 64th byte fell inside a character panicked the actor: `/add`
  with 22 CJK characters (66 bytes) was enough, on the Tox transport. The prefix is now
  taken in characters, so a value shorter than a key is returned unchanged and a longer
  one is cut on a boundary. Covered by
  `transport::tests::test_public_key_prefix_never_splits_a_character`.

- **A peer connection is now bounded in both directions: how long it may hold a slot,
  and how much it may make the sender hold.** Two faults were reachable by a remote
  party, and neither needed the passphrase.

  - **A silent socket held a connection slot forever.** The peer transport had a
    deadline for an outbound *dial* but none for the *greeting*: a socket that
    completed the TCP handshake and then sent nothing stayed in the peer registry
    until it closed the socket itself. With `max_connections` such sockets, an
    attacker — or a client that crashed and whose FIN was swallowed — took the
    instance off the air for everybody else. A peer now has
    `[network] connection_timeout` (30 s by default, the same value that bounds a
    dial, because both answer "how long may this connection stay useless") to announce
    a nickname, after which the slot is reclaimed and `PeerDisconnected` is published.
    Pinned by `network::tests::test_a_peer_that_never_greets_loses_its_slot`, which
    connects a raw socket that sends nothing.
  - **A peer that stopped reading grew the sender's memory without bound.** Each
    connection's outbound queue was an unbounded `mpsc::channel`: once the peer's
    socket buffer filled the writer task blocked, and every frame the core produced
    after that accumulated in memory at a rate the remote party chose. The queue is now
    bounded (`PEER_QUEUE_CAPACITY`, 256 frames) and `try_send`-based: a full queue
    sheds and increments a counter that `/metrics` reports as `payloads_dropped` —
    which until now was documented as Tox-only and hardcoded to `0` for TCP. A shed
    frame is *not* counted as a peer the ciphertext was queued for, so the send report
    carries `queued_for: 0`, which the front-end already renders as "queued for no
    peers connected": the frame itself is lost, and the counter plus that report are
    its only trace. Pinned by
    `network::tests::test_a_peer_that_stops_reading_sheds_instead_of_growing`, which
    asserts where shedding starts (at the capacity, not earlier and not never) rather
    than only that it happens — verified against a probe that printed the exact
    boundary, and by mutating the counter away (the test then fails).
  - What is still *not* bounded is an *established* idle connection: without a
    keepalive frame a half-open peer (a machine that vanished without a FIN) is only
    noticed when a write fails. That needs a protocol addition, so it is now recorded
    as open work (`docs/ARCHITECTURE.md` §6.20) instead of being implied by a silence.
    `README.md` (networking model, known limitations), the `connection_timeout` field
    doc and `config.toml` say which deadline is which.

- **Every shipped path is now free of panic-prone constructs — and stays that way.**
  The audit had two halves. First the code: the only two `expect`s left in shipped
  code were the stdin-reader thread spawns in both front-ends (a front end whose
  thread cannot be created panicked instead of reporting), and the only index peer
  input could reach was the message-id split in `network.rs`, which was safe *only*
  because `decode_message_id` had already proved the prefix present — an invariant the
  next edit to that function could have broken. The spawns now return the `io::Error`
  to `run`, the split is a checked `payload.get(..)` that logs and drops the frame,
  `commands::parse` uses `strip_prefix('/')` instead of a byte slice that was safe only
  because `/` is ASCII, and `CoreService::resolve_group` matches `[only]` on the slice
  instead of indexing `groups[0]` behind a length check. The other nineteen indexing
  sites were audited individually and are fixed-size arrays, `.get(..)`-guarded or
  `.position()`-derived; the ratatui layout chunks are the one case whose length comes
  from the constraint count, which is now stated where it is relied on. Second the
  enforcement: the crates deny `clippy::unwrap_used`, `expect_used`, `panic`,
  `unreachable`, `todo` and `unimplemented` (with a `cfg(test)` allow, since a test may
  assert by unwrapping), and CI and `make clippy` pass `-D warnings`, so a new panic
  path fails the build instead of surviving review. Verified by probe: a `unwrap()`
  added to shipped code is reported as `clippy::unwrap_used`, while the several hundred
  unwraps in unit tests stay silent.

- **`tox.rs` now carries a safety argument for every one of its 55 `unsafe` blocks.**
  Six of the ten `user_data` dereferences in the toxcore callbacks had no `SAFETY`
  comment (the other four were covered by a neighbouring one), so the argument for
  reaching through the opaque pointer was implicit in half the callback set. Each site
  states it now and points at the canonical version, and an audit reports zero unsafe
  blocks without an argument in the surrounding lines. The callback buffers were
  already correct — every `from_raw_parts` is guarded by `is_null() || length == 0`,
  the case that is undefined even for a zero length — and §4's "no `unsafe`" invariant
  was corrected to what is actually true: no `unsafe` **outside** the FFI module, which
  is the single exemption.

- **Documentation drift, and a CI-parity gap in `make ci`.** Five statements in the
  shipped documents no longer matched the code: the compatibility-window item in
  `docs/ARCHITECTURE.md` §6.4 still said `PROTOCOL_VERSION` was 7 (it is 8), §6.7
  counted nine `logging::tests` (there are ten — the list next to it was always ten),
  the §3.2 handshake example and the `README.md` frame example both showed a stale
  `protocol_version` literal, `README.md` listed six `[config.toml]` sections and left
  out `[ipc]`, and the `/requests` row omitted its `/pending` alias. `Cargo.toml`'s
  database comment claimed the default build enables SQLite, while every crate
  declares `default = []`, so a plain build compiles no driver at all. Separately,
  `make ci` ran three of the four CI jobs: the docs job
  (`RUSTDOCFLAGS="-D warnings"`) had no local equivalent, so a rustdoc warning that
  fails CI passed `make ci`. There is now a `doc-check` target (part of `ci`), and
  `README.md`/`CONTRIBUTING.md` show the four commands the pipeline actually runs.
  The audit also recorded, instead of leaving unmentioned, that `.cursorrules`
  declares three capabilities the tree does not implement (file transfer, chain
  integration, signatures) and one that holds in a different form (user
  authentication is the shared passphrase and IPC token, not an account) —
  `docs/ARCHITECTURE.md` §6.19.

- **The response envelope is now documented and pinned byte for byte.** §3.4 of the
  interface document and the frame example in `README.md` showed the success shape
  only, and nothing in the tree asserted the *literal* the wire uses — the failure tag
  is `err`, not `error`, so a client written in another language had to read the Rust
  source to learn it (found by driving the endpoint from a `socket`+`json` script, the
  cross-language path the README advertises). `ipc_socket_test::test_the_wire_literals_are_what_a_foreign_client_reads`
  now speaks the protocol with a bare `TcpStream` and asserts the bytes of the
  `hello`/`welcome` handshake, the success envelope, the failure envelope and that the
  socket still serves the next request after a refusal; it fails if the tag is renamed
  (verified by mutating the `serde` attribute and reverting). Both documents carry the
  two forms.

- **Seven defects the full-matrix run, a re-read of the key-agreement change, an
  audit sweep for unbounded growth and an end-to-end pass over the REPL turned up.**
  None of them changed the wire format; three were in the paths around the
  agreement, two were tests that raced the thing they were testing, one let a peer
  drive the disk, and one kept `/quit` from ending the process.

  - `NetworkManager::send_to_peer` (the convenience that takes *plaintext*) sealed
    every payload with the **session** key, while the documentation above it — and
    the test named `test_a_directed_message_uses_the_pair_key` — said a directed
    message is sealed under the pair's key. The core was always right (it seals with
    `contact_key` and hands the ciphertext to `send_payload`), so this was a
    mislabelled convenience, not a leak; it now applies the same rule the core does
    (`pair_keys`: the agreed key, else the static derivation, else the session key),
    which also means the tests that use it exercise the agreement for real.
  - `Shared::accept_ephemeral` overwrote the peer's ephemeral whenever another
    `FRAME_EPHEMERAL` arrived. A connection is agreed **once**, so a repeated or
    tampered frame could re-key a connection the peer was still using the first key
    on (its frames would then stop decrypting). The first *usable* announcement now
    wins and a later one is ignored and logged at `debug`, which makes the failure
    mode closed rather than quiet.
  - `PinStore::observe` wrote the whole pin file on **every** identity change, and a
    peer can produce a change per frame (announce two identities alternately), so a
    member of the session could drive arbitrary disk writes from the network. A
    nickname is now written when it is first seen and once more when it changes, and a
    later change is kept in memory (still reported, still counted) instead of
    rewriting the file. The write rate is therefore a function of the table's size
    (`PIN_CAPACITY`), like the Tox queue's, instead of the frame rate (R7). The cost is
    that the file can hold an earlier key, so a restart reports that change again —
    a repeated warning, not a missed one. Covered by
    `trust::tests::test_a_flood_of_changes_does_not_rewrite_the_file`, which fails if
    the bound is removed.
  - `CoreService::start` set the nickname, the identity and the status *after*
    binding the listener, so a peer that connected inside that window was greeted
    with an empty nickname and no identity (and a frame could arrive while the
    identity was being written). The three are now set before the transport starts,
    so the first greeting is always complete.
  - `network::tests::test_a_directed_message_uses_the_pair_key` waited for the
    peer's *nickname* before asserting a pair key. The nickname arrives with
    `hello` and the identity one frame later, so the assertion raced the greeting —
    it passed by luck until the greeting grew a third frame (the ephemeral) and lost
    the race under the default feature set. It now waits for both ends to be past
    the static derivation, i.e. for the key the connection actually agreed, and the
    new `test_the_three_step_ladder_on_a_connection` pins the receiver's half of the
    ladder on a real connection (agreed key, static key, session key).
  - The line-oriented REPL and the TUI's fallback reader read stdin through
    `tokio::io::stdin()`, which performs the read on tokio's **blocking pool**.
    Dropping the runtime waits for that pool to drain and the read is not
    cancellable, so the process stayed alive after `/quit` (or Ctrl+C) whenever
    stdin was still open — an interactive terminal, or a parent process holding the
    pipe. The test suite hid it because `execute` closes stdin and `RunningInstance`
    kills the child. Both readers now run on a plain OS thread, which the runtime does
    not join, and EOF or a closed queue still stops them. Reproduced on a pty, where
    the process never returned to the shell, and pinned by two end-to-end tests that
    keep stdin open and assert a clean exit: `test_two_run_instances_exchange_messages`
    for the plain REPL and `test_tui_fallback_quits_with_stdin_open` for the TUI's
    fallback reader.
  - `repl_commands_test::test_two_run_instances_exchange_messages` wrote `/peers`,
    `/msg` and `/quit` as one script, so the client could run `/msg` before its dial
    completed: the message was buffered for a peer that was still *offline* to the
    client and then dropped when the script's `/quit` ended the process. That is the
    script racing the handshake, not the delivery path the test exists to prove. The
    client is now kept resident and addresses the peer only once it has seen it
    connect, the server's printed frame is awaited, and the client's own clean exit is
    asserted. It failed 2 runs in 5 on a loaded machine before the change.


- **The documents quoted a protocol version the build does not speak, and nothing
  compared the two.** `PROTOCOL_VERSION` is 9, while the frame example in
  `README.md` and §3.2 of `docs/ARCHITECTURE.md` still showed a version 8 handshake
  and §6.4's compatibility-window item said `PROTOCOL_VERSION` was 8. The README is
  what the document tells a client in another language to copy ("a client speaks
  JSON frames with a 4-byte big-endian length prefix … any other language can drive
  the backend"), and the same two sentences had already drifted once (7 → 8) — so
  the finding is the missing *gate* rather than the wrong digit.
  `doc_contract_test` now reads `README.md` and `docs/ARCHITECTURE.md` from
  `CARGO_MANIFEST_DIR` (the pattern `repl_commands_test` already uses for the
  shipped `config.toml`) and asserts, against the constants, every
  `protocol_version` field in a handshake example, every `` `PROTOCOL_VERSION` is N ``
  / `` `MIN_SUPPORTED_PROTOCOL_VERSION` is N `` prose claim, the header's window
  (`protocol version 9 (serves 3..=9)`), the length-prefix claim, the §4 table's
  `Frame size` cell (`1 MiB`, from `MAX_FRAME_LEN`), the message ceiling
  (`32 KiB`, from `config::MAX_MESSAGE_LENGTH`) and the shipped default
  `app.max_message_length`. It also reads the README's CLI option table and asserts
  every option in it against the binary's `--help` output, so a renamed flag cannot
  leave the user-facing reference describing a command line that does not exist
  (the mirror image of the `--history-limit` defect, where a documented flag reached
  no code). Every assertion names the document, the line, the value it found and the
  constant, and each one was verified to fail by reverting that single claim.
  Recorded as A52 in `docs/ARCHITECTURE.md`.
- **A default-feature build warned about `TuiManager`'s stored `[ui]` options.**
  `options` is read only by the full screen interface, so a build without
  `terminal-ui` — the one `cargo build --workspace` and the CI test job compile —
  reported `field 'options' is never read`. The field is still stored there (the
  constructor's signature deliberately does not change with the feature), so the
  warning is filed as what it is, with the reason, next to the `dead_code` allow
  `tox.rs` already carries for its FFI-only payloads:
  `#[cfg_attr(not(feature = "terminal-ui"), allow(dead_code))]`. Every feature
  combination now builds without a warning from this workspace
  (default, `sqlite`, `terminal-ui`, `tox-protocol`, `sqlite,terminal-ui`,
  `--all-features`, and `--all-targets`).

### Added

- **Established peer connections are now probed: `FRAME_KEEPALIVE` and an idle
  deadline.** TCP reports nothing on a read while no data arrives, so a peer that
  vanished without a FIN — a machine that lost power, a NAT that dropped its mapping —
  was indistinguishable from an idle one: it stayed in the peer registry, `/peers`
  listed it, and a message addressed to it was written into a black hole until the
  kernel's retransmit timer gave up, minutes later. Each connection now sends an empty
  keepalive frame (kind 8) to every greeted peer every 20 s from a single manager-wide
  task, and `read_loop` drops a connection that has received *nothing* for 60 s (three
  cadences, so one delayed frame cannot tear down a healthy link), naming the reason in
  `PeerDisconnected`. The deadline arms only once the peer has *sent* a keepalive: a
  peer built before the frame kind ignores it — the same rule that made
  `FRAME_IDENTITY` and `FRAME_EPHEMERAL` compatible — and therefore keeps the old
  behaviour instead of being dropped for being quiet, so half-open detection is a
  property of a modern pair. No `PROTOCOL_VERSION` change was needed: the peer frame
  kinds are the transport's own contract (§3.11) and `PROTOCOL_VERSION` governs the
  front-end/core boundary, which the previous note on this work (§6.20) had got wrong.
  Covered by three tests, each verified to fail when its mechanism is removed (arming
  unconditionally, never arming, and not sending):
  `network::tests::test_a_greeted_peer_is_sent_keepalives` (the sending half, read off
  the wire), `test_a_silent_peer_is_dropped_once_it_has_proved_it_sends_keepalives`
  (the receiving half, including the reason on the event) and
  `test_a_peer_that_never_sends_a_keepalive_is_not_dropped_for_silence` (the
  compatibility rule). `docs/ARCHITECTURE.md` §6.20 now records it as closed with the
  asymmetry, and `README.md` documents the cadence and what is *not* tunable.

- **`examples/` — one runnable program per application case.** The workspace had a
  test suite and doc examples but no example directory, although `.cursorrules`
  declares one and `README.md`'s snippets were the only place a library user could see
  the public API in use. Seven cases now exist, six of them in the default build and
  the Tox one behind `required-features = ["tox-protocol"]`: `embed_core` (embed the
  headless service and drive it with `Request` / `Reply` / `CoreEvent`), `repl_session`
  and `tui_session` (attach either front-end to an embedded core), `peer_chat` (two
  cores exchanging an encrypted message over loopback), `remote_frontend`
  (`RemoteClient`: the same contract over the IPC socket, including the server's
  refusal of a remote `Shutdown`), `pin_identity` (the announced identity, the pair
  secret both ends derive, and the trust-on-first-use pin table) and `tox_transport`
  (the Tox transport directly, reporting `ToxError::Unavailable` as a value when
  `libtoxcore` is missing). `cargo test --workspace` builds every example, so one that
  stops compiling fails the default CI run, and `cargo clippy --all-targets` lints
  them; they follow the test suite's hermeticity rule (temporary data directory,
  loopback addresses, nothing written into the working tree). `README.md` and
  `docs/ARCHITECTURE.md` §7.1 map the cases to the files, and §6.18 records the
  `benches/` gap that is still open next to it.

- **Per-pair keys are now agreed, not derived: the confidentiality half of A12.**
  The per-contact key used to be `HKDF-SHA256(session key, label, ordered pair of
  identities)`, so anybody who held the passphrase and had seen both announced
  identities — which is every other member of the session, and a passive listener to
  the handshake — could reproduce it. Each connection now contributes an ephemeral
  X25519 key pair and the pair derives a **three-DH** key (RFC 7748):

  - a new frame kind `FRAME_EPHEMERAL` (7) carries this connection's ephemeral public
    key (64 hex, the same shape as the identity) right after `FRAME_IDENTITY`, once
    per connection. A peer that does not know the frame logs it as unknown and keeps
    working on the static derivation, so the change is compatible in both directions.
  - `identity::pair_key` is `HKDF-SHA256(session key, "metaText/contact-key/v2",
    DH(S,S) ‖ DH(E,S) ‖ DH(S,E))`. The two ends compute the same three values because
    `DH(x, Y) == DH(y, X)`; the order is pinned by the pair of static keys, so no
    message beyond the two announcements each end already sends is needed.
  - What it buys: an observer (session key + both announced public keys) can no
    longer reproduce a pair key, because one input needs a static **secret**; the key
    is fresh per connection, so a recorded handshake does not open a later one; and
    the session key is still the HKDF salt, so a peer without the passphrase gets
    nothing.
  - What it does not buy: the identity is announced, not signed, so a *first* contact
    is trust-on-first-use. An active participant can present its own identity and
    relay, which the pin reports (`peer_identities_changed`, `⚠️ changed since it was
    pinned`) and a fingerprint comparison detects — but which only an off-channel
    authenticated identity could prevent. `docs/ARCHITECTURE.md` §6.2 now records that
    narrower question instead of the whole finding, and A12 is closed.
  - A directed message is sealed under the first key that exists and a receiver tries
    them in the same order (`network::pair_keys`): the **agreed** key, then the
    **static** derivation for a peer that announced no ephemeral (an older build) or
    whose announcement was unusable, then the **session** key when the pair has no key
    of its own — a broadcast, or a payload buffered while the peer was offline. The
    derivations are domain-separated by their labels, and an announcement that is not
    a key is treated as *no* announcement rather than as a weaker scheme.
  - `CoreTransport::set_identity` / `NetworkManager::set_identity` now take the whole
    `NetworkIdentity` (the secret half is what the agreement uses locally); the wire
    still carries only the public half.
  - Covered by `identity::tests` (five new: symmetric, per-connection, *not* the
    observable derivation, a substituted identity does not yield the pair's key, the
    ephemeral's `Debug` is redacted), `network::tests::test_the_pair_key_ladder` (the
    agreed key first, the static fallback for no/unusable ephemeral, nothing for a
    pair without an identity) and
    `network::tests::test_a_directed_message_uses_the_pair_key`, which now also
    asserts that the key the two ends hold is not the one an observer of the
    handshake can compute. See `docs/ARCHITECTURE.md` §3.11.

- **The network identity persists, and a peer's identity is pinned: the identity
  half of A12.** The identity an instance announced was `hex(random)` per session,
  which is what made the per-contact key half unverifiable — a key derived from a
  value that changes every run separates two pairs but cannot be *checked* by either
  of them, so it could never be pinned (and the TCP outbox could not be keyed by it;
  see `docs/ARCHITECTURE.md` §6.17). It is now a persisted X25519 key pair:

  - `net-identity.json` in the data directory holds the secret half (created
    owner-only where the platform has modes, written by atomic replace, read at
    startup, replaced with a warning when it is corrupt or a layout this build does
    not know). The **public half** is what is announced, still 64 hexadecimal
    characters, so no frame changed and a peer needs no new code.
  - `NetworkIdentity::fingerprint` is `SHA-256(public key)[..16]` in eight groups of
    four. `/whoami` and `/info` print it, so two users can compare identities out of
    band — the comparison a signature-less announcement otherwise lacks.
  - A peer's announced identity is remembered in `net-peers.json`, keyed by the
    nickname it announced (the only handle TCP has), bounded to 1024 entries with
    the excess counted, written sorted so a diff of the file is a diff of what
    changed. A **change** under a known nickname is reported rather than refused:
    a `warn` line naming both fingerprints, `peer_identities_changed` in
    `/metrics`, and `changed` on that peer in `/peers` (`⚠️ changed since it was
    pinned`). `peer_identities_refused` counts announcements that could not be
    pinned at all. See §3.10.
  - `NetworkIdentity::agree()` is the Diffie-Hellman step the key agreement uses; the
    entry above records what it is used for. The persisted identity does not have to
    change shape to be signed later, because the public value a fingerprint names is
    already a real key.
  - Protocol version 8 carries the additions (`SessionInfo::identity_fingerprint`,
    `SessionInfo::peer_identities`, `PeerIdentityView`, and the two `MetricsView`
    counters). Every new field is `#[serde(default)]`, so a version 3 front-end
    decodes the snapshot unchanged.
  - Covered by `identity::tests` (12: the key pair and announced shape, the
    fingerprint, a damaged secret and a future layout refused, the identity
    surviving a restart, owner-only permissions, a redacted `Debug`),
    `trust::tests` (13: first sighting / unchanged / change, the change counted and
    flagged per peer, a pin surviving a restart with a change still detected across
    it, the bound with the excess counted, case-insensitive nicknames, a stable file
    order, a volatile store),
    `network::tests::test_a_peer_identity_is_pinned_and_a_change_is_reported` (a real
    connection: the transport feeds the store from `FRAME_IDENTITY`, and a re-announced
    identity is flagged) and
    `core_service_test::test_the_identity_survives_a_restart` (two cores over one
    data directory announce the same identity and fingerprint).

- **The Tox queue survives a restart: a pending friend request and a payload
  buffered for an offline friend are now persisted.** Two things a user can
  observe lived only in memory, and a restart lost both: an incoming friend
  request (`toxcore` keeps no record of it in the savedata — the request *is* the
  callback, and "not answered" is the refusal, so a request the user had read but
  not answered could no longer be answered) and the outbox for an offline friend
  (`queued` quietly meant "queued until I exit"). Both now live in
  `tox-state.json`, next to `tox-savedata.bin` in the data directory: a bounded
  JSON snapshot keyed by **public key**, because a friend number is assigned in
  the order the savedata lists friends and would name the wrong peer after an
  unrelated removal. The file is rewritten after every mutation of either list —
  atomically, and serialised between the actor and the bridge so two concurrent
  writers cannot publish the older snapshot last — so a restart reads back exactly
  what a front-end was last told. Restoring applies the same rules as the live
  path: the TTL is now measured on a wall clock (a queued payload's `queued_at` is
  a `SystemTime`, not an `Instant`, because it has to age across a restart), a
  queue is capped per friend keeping delivery order, a request is deduplicated and
  capped, a payload whose peer is not a friend (any more) or whose entry is
  unreadable is dropped and counted, and a timestamp in the future is treated as
  "just queued" rather than expired. A missing, corrupt or newer-version file
  yields an empty queue with a warning and never a failed start: the identity
  lives in the savedata, and the queue is a durability extra. The bridge's prune on
  a connection event also counts into `payloads_expired` now — it had logged the
  drop without counting it, which made `/metrics` disagree with the message the
  sender was told was waiting. Covered by `tox_store::tests` (layout, atomic
  replace, corrupt and future-version files, bounds),
  `transport::tests::{test_the_queue_round_trips_through_the_store,
  test_a_payload_that_expired_while_down_is_not_restored,
  test_a_payload_that_cannot_be_addressed_is_dropped,
  test_a_restored_queue_is_capped_keeping_the_delivery_order,
  test_restored_requests_are_bounded_and_deduplicated,
  test_a_timestamp_in_the_future_is_treated_as_just_queued}`,
  `transport::tests::test_a_queued_payload_survives_a_restart` (a real
  `ToxTransport` restarted over the same data directory, no network needed) and the
  opt-in `test_a_pending_friend_request_survives_a_restart_over_tox`, which is the
  only end-to-end proof of the request half: a pending request can only be produced
  by a second instance over the real DHT.

- **Per-contact key material: a directed message now uses a key derived for the
  pair, not the session key.** The TCP transport had exactly one key for the whole
  session, so every peer that knew the passphrase could read every frame — including
  one addressed to somebody else — and a frame captured from one pair could be
  replayed as part of another. Each instance now announces its DID right after the
  nickname (`FRAME_IDENTITY`, one frame per connection), and `CryptoManager::contact_key`
  derives HKDF-SHA256(session key, label, ordered pair of DIDs): both ends compute the
  same key, and a message addressed to one peer is sealed under it. The derivation is
  ordered and case-insensitive, so either side and either hex case agree; it is
  deterministic, so there is no cache to go stale. Broadcasts, and anything directed
  at a peer that announced nothing (an older build, or a message buffered while the
  peer is away), still use the session key, and the receiver tries the pair key first
  and then the session key, so a mixed-version pair keeps talking. **What this is
  not**: the derivation input is a shared secret, so a passphrase holder who has seen
  *both* DIDs derives the same key — per-contact *confidentiality* against that needs
  real identities and a key exchange, which stays open as A12. Covered by
  `crypto::tests::test_contact_key_is_a_key_per_pair`,
  `test_contact_key_refuses_empty_identities`,
  `test_encryption_under_a_contact_key`,
  `test_explicit_keys_degrade_when_encryption_is_disabled`,
  `network::tests::test_a_directed_message_uses_the_pair_key` and
  `network::tests::test_a_peer_without_an_identity_uses_the_session_key`; the whole
  end-to-end suite (two real binaries, `repl_commands_test`) now exercises the pair
  key path, because the core announces its identity at startup.
- **A group invitation can now be refused.** `Request::DeclineGroupInvite` /
  `Reply::GroupInviteDeclined` and `/group decline <token>` (alias
  `/group reject`) discard a pending invitation without joining anything, which
  is the invitation-side counterpart of `/reject` on a friend request. The list
  that holds invitations is bounded
  (`transport::PENDING_INVITES_CAPACITY`), so before this the only way to make
  room for the invitation a user actually wanted was to join a conference and
  leave it again. Nothing is sent to the inviter — toxcore has no "decline", an
  invitation is a capability handed to us — so declining is a local decision; it
  is also not a block, and the same peer can invite again. Protocol version 7.
- **A pending friend request can now be refused.** `Request::RejectPeerRequest` /
  `Reply::PeerRequestRejected` and `/reject <index|public key>` (alias
  `/decline`), with `ToxTransport::reject_request` behind them. Before this the
  only answer was `/accept`, which was already awkward and became a wedge once the
  pending list was bounded: the only way to make room was to become friends with
  whoever had filled it. Nothing is sent to the requester — toxcore has no reject
  message, and a request only becomes a friendship when it is accepted.
- **Group invitations are readable over the protocol (v6).**
  `Request::GroupInvites` / `Reply::GroupInvites` list the invitations waiting for
  an answer, reusing `GroupInviteView`. Only the `group_invite_received` event
  carried the join token before, and a broadcast event is only seen by a front-end
  that was already attached — so a session that attached later could read
  `pending_group_invites` in `/info` and had no way to act on it. `/group invites`
  prints each inviter and the token on its own line, and `Reply::GroupInvites`
  carries `supported: false` on a transport without groups, like `Reply::Groups`.
- **Group rename (v6).** `Request::RenameGroup` / `Reply::GroupRenamed` and
  `/group rename [@group] <name>` change a conference's title, which belongs to the
  *group*: the transport forwards it and every participant sees the new name. The
  transport layer gained `ToxClient::conference_set_title` /
  `ToxSender::conference_set_title` for it. Because our own instance receives no
  `conference_title` callback for its own change, the reply and the core state are
  what make it visible locally, and the opt-in DHT test is what proves the other
  side actually sees it.
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
- **Tox foundation layer** (`src/tox.rs`, feature `tox-protocol`): hand written
  FFI bindings to the system `libtoxcore` plus a threaded `ToxClient`
  (`ToxConfig`, `ToxEvent`, `ToxFriend`, `ToxSnapshot`, `ToxIdentity`,
  `ToxCommand`), mirroring the original C `metaText` flow: create the instance,
  bootstrap the DHT, `tox_friend_add`, `tox_friend_send_message`, and the
  self/friend/message callbacks. `toxcore` is single-threaded, so the instance
  is owned by a dedicated worker thread and driven through command/event
  channels. `build.rs` locates and links the library; when it is absent the
  module compiles to a stub whose `ToxClient::start` returns
  `ToxError::Unavailable`, keeping `--all-features` green without toxcore.
  The pure-Rust `tox` crate dependency was dropped (GPL-3.0+, tokio 0.2, no
  client API). Not yet wired into the actor or the CLI.
- **Tox transport in the application** (`--transport tox`, feature
  `tox-protocol`): a REPL front-end (`ui::tox`) that drives the Tox instance
  directly. It prints the local Tox address on startup, sends friend requests
  with `/add`/`/connect`, accepts incoming ones with `/accept` (a new
  `Command::Accept`), lists friends and their connection state with
  `/peers`/`/list`, and chats with `/chat`, `/msg` or plain text. The identity
  lives in toxcore's own savedata, so the address survives a restart. DHT
  bootstrap nodes from the C reference are built in
  (`tox::default_bootstrap_nodes`), and `--peer` accepts a 76 character Tox
  address in this mode. `--bootstrap` is rejected for the tox transport because
  it needs a public key that a `host:port` cannot carry. `ToxSender` (a
  `Clone + Send + Sync` command handle) lets the async front-end drive the
  single-threaded toxcore worker through `spawn_blocking`.
- **Tox hardening.** toxcore's own limits are enforced before anything is
  created: `ToxConfig::validate` checks the UDP port range and the nickname /
  status message lengths (`TOX_MAX_NAME_LENGTH`,
  `TOX_MAX_STATUS_MESSAGE_LENGTH`), `add_friend` checks the request body
  (`TOX_MAX_FRIEND_REQUEST_LENGTH`) and `send_message` the body length. A zero
  byte savedata file is treated as "no identity yet" (toxcore rejects it with
  `LOAD_BAD_FORMAT`, which used to make the transport unstartable), and savedata
  toxcore genuinely refuses to load is reported as `ToxError::Savedata` instead
  of silently generating a new identity. The error type now carries precise
  variants (`BadKeyLength`, `TooLong`, `EmptyMessage`, `PortRange`, `Savedata`)
  instead of magic toxcore codes.
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
- **Per-client request rate limiting on the IPC endpoint.** The bounded queues of
  §3.8 limit *memory*, not the work a socket client can make the core do. A
  per-connection token bucket (`ipc::server::TokenBucket`, 256 req/s with a 512
  burst) now sheds a flood with the stable `backpressure` error code and detaches
  a client that keeps flooding (256 strikes). The shed reply travels through the
  same ordered queue as real replies, so the documented request/reply ordering
  still holds.
- **A non-loopback `--ipc-listen` now requires `--ipc-token`.** Previously an
  unauthenticated endpoint on a public interface only logged a warning, so a typo
  could expose the backend to the network. Startup is refused instead;
  `utils::is_loopback_host` decides (IPv4 `127/8`, IPv6 `::1`, `localhost`), and a
  loopback bind without a token still works and warns.
- **Tox outbox.** A payload addressed to a known-but-offline Tox friend is no
  longer reported as dropped: it is buffered per friend (32 payloads, 5 minute
  TTL, expired entries pruned on every tick) and flushed in order when the friend
  comes online. This gives the Tox transport the same delivery contract as TCP,
  and `/info` now shows the waiting count for both.
- `ToxSender::send_message_bytes` / `ToxClient::send_message_bytes` for sending an
  opaque payload, and `ToxTransport::expired_messages` for diagnostics.
- `tests/ipc_socket_test::test_request_rate_limit_sheds_a_flood`,
  `tests/repl_commands_test::test_non_loopback_endpoint_requires_a_token`,
  `server::tests::test_token_bucket_*` and `utils::tests::test_is_loopback_host`.
- **Message kinds (`/me` third-person actions).** `MessageKind` (`text` /
  `action`) is now carried end to end: a `/me <action>` reaches the transport,
  which tags it the way its own protocol does — a dedicated TCP frame kind
  (`FRAME_ACTION`, same body layout so an older peer reports it as unknown rather
  than misreading it) and toxcore's `TOX_MESSAGE_TYPE_ACTION` — and both the
  sender and the peer render it as `* <nick> <action>` instead of a chat line.
  Previously the Tox message type was received and then discarded, so an action
  from another Tox client was displayed as an ordinary message.
- **Protocol compatibility window (v4, serves 3..=4).** The endpoint now accepts
  every version in `MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION` instead of
  exactly one, so a front-end can be upgraded independently of the core. The
  rejection for anything else names the window. `MessageKind` was added with
  `#[serde(default)]`, so a version 3 client that never sets `kind` keeps working
  unchanged — which is what makes the window meaningful rather than nominal.
- `ToxMessageKind::as_raw`, `ToxSender::send_message_typed` and
  `ToxClient::send_message_typed`, so a caller can pick the toxcore message type.
- Tests: `message_exchange_test::test_action_messages_keep_their_kind` (an action
  and a message with the same body stay distinguishable),
  `test_a_burst_of_messages_arrives_complete` (200 messages, none lost, none
  reordered), `test_received_messages_are_in_the_history_after_a_restart`
  (durability across a restart), `repl_commands_test::test_me_action_is_sent_and_rendered`
  (the binary, sender and peer rendering), `ipc_socket_test::test_the_compatibility_window_is_served`
  and `tox::hex_tests::test_message_kind_round_trips`.
- **Operational metrics (`/metrics`).** `Request::Metrics` → `Reply::Metrics`
  (`MetricsView`) returns one non-blocking snapshot: uptime, transport, peers
  connected/pending, payloads queued/expired/dropped, friends and their limit,
  pending friend requests, request-queue depth *and* capacity, event subscribers,
  traffic counters and the persistence state. The REPL/TUI render it as
  `key=value` lines so it can be read by a person and scraped by a script.
  Previously only `/stats` existed, which showed traffic and nothing about
  saturation.
- **Message kind in history.** `messages.kind` is persisted, returned in
  `MessageView` and rendered as `* <peer> <action>`, so a restored `/history`
  reads the way the session did. An existing database is upgraded in place: the
  column is added by a `PRAGMA`-guarded `ALTER TABLE`, and rows written before it
  existed decode as ordinary messages instead of failing the query.
- **Content types (`/bin`): binary payloads are a first-class message.**
  `types::ContentType` (`text` / `binary`) now travels with a message from the
  request to the rendered line:
  - `Request::SendMessage` and `CoreEvent::MessageReceived` gained
    `content_type` (`#[serde(default)]`, so a version 3 client keeps working),
    and `MessageView` / `messages.content_type` carry it through history.
  - A binary body is carried over the JSON interface as lowercase hexadecimal,
    so the protocol stays text-only, and it travels over the transports as real
    bytes: a dedicated TCP frame kind (`FRAME_BINARY`, same body layout as a
    chat frame) and toxcore's own byte-array message.
  - A receiver never *guesses wrong*: a declared binary body is not decoded as
    UTF-8 and therefore never reported as undecodable; it is rendered as
    `[binary, N B] <hex preview>` live and in `/history`. The Tox transport has
    no content-type field, so the receiving bridge classifies the body by UTF-8
    validity — which is exactly the information a Tox peer can convey.
  - `ipc::validation::binary` rejects an empty body, an odd number of digits, a
    non-hexadecimal character or a body larger than `max_message_length`
    *after* decoding, before any crypto or routing happens; `(action, binary)` is
    refused as a contradiction. A refused request changes no state.
  - `messages.content_type` is added to an existing database by the same guarded
    `ALTER TABLE` as `messages.kind`, and a legacy row decodes as `text`.
- **The Tox callback hand-off is bounded.** The toxcore → bridge channel became a
  `sync_channel` (`tox::EVENT_QUEUE_CAPACITY`, 4096) fed with `try_send`: a
  callback can no longer grow memory without limit, and a shed event increments a
  counter exposed as `payloads_dropped` (`ToxSender::dropped_events`). This was
  the last unbounded queue in the design.
- Tests: `message_exchange_test::test_metrics_reflect_the_exchange` and
  `test_metrics_track_a_buffered_payload`, `ipc_socket_test::test_metrics_are_available_over_the_wire`,
  `database::tests::test_message_kind_round_trips_through_the_database` and
  `test_migration_adds_the_kind_column_to_a_legacy_table`,
  `tox::tests::test_a_quiet_instance_sheds_nothing` and
  `tox::hex_tests::test_event_queue_capacity_is_bounded_but_generous`.
- **Actor lag in `/metrics`.** `MetricsView` gained `request_wait_last_us` /
  `request_wait_max_us` (the queueing delay a front-end actually experienced),
  `request_service_last_us` / `request_service_max_us` and `requests_served`.
  `Command` carries the `Instant` it was enqueued at, so the wait covers the
  actor being busy with a slow subsystem and not only a full queue — the signal
  that distinguishes a busy actor from a stuck one, which the traffic and depth
  counters could not. Both maxima are kept so a transient stall is not averaged
  away.
- Tests: `core_service_test::test_metrics_report_actor_lag` (a single request is
  counted and timed; a burst of 16 queued requests reports a non-zero wait; the
  service time is measured on a database round trip, because an idle actor serves
  a ping in under the clock's resolution), `commands::tests::test_parse_binary_command`,
  `network::tests::test_message_frame_kinds`, `ipc::validation` tests,
  `message_exchange_test::test_binary_payload_round_trips` and
  `test_invalid_binary_body_is_refused_without_side_effects`.
- **Group chat over Tox conferences.** A group is a first-class conversation in
  the protocol, the core and the history — not a separate front-end:
  - `types::Group` (keyed by the transport's **stable** id, never by a locally
    invented one) and `AppEvent::{GroupChanged, GroupMessageReceived,
    GroupInviteReceived}` replace the dead `GroupEvent`/`GroupEventType`
    scaffolding, which had no producer and carried a `Uuid` no transport could
    ever have supplied.
  - Protocol v5 adds `Request::{Groups, CreateGroup, JoinGroup, InviteToGroup,
    SendGroupMessage, LeaveGroup}` with matching replies, and
    `CoreEvent::{GroupMessageReceived, GroupChanged, GroupInviteReceived}`.
    `SessionInfo` and `MetricsView` advertise `supports_groups` /
    `groups_supported`, so a front-end never has to guess.
  - `tox_conference_*` is bound: create (with a title), join (from a single-use
    invitation token), invite, send, leave, peer counts and titles. The
    per-instance conference number never leaves `tox.rs` — only the stable id —
    so a group still resolves after toxcore rebuilds its numbers from savedata.
  - `transport::CoreTransport` gained the group surface for both variants. On TCP
    every group mutation fails with a typed, actionable refusal ("the tcp
    transport has no groups … start with --transport tox") while `Request::Groups`
    *succeeds* with `supported: false`, because "there are none" and "there can
    never be any" are different answers.
  - Group messages reuse the whole direct-message pipeline: the same validation,
    the same `content_type` rules (a binary body is hexadecimal, never
    undecodable), the same statistics, the same event stream. They are persisted
    with `messages.conversation` (`direct` / `group`) and `messages.group_id`, and
    render as `👥 [Team] Alice: hello group`.
  - `/group` (`/g`) drives it: `list` (default), `create`, `join`, `invite`,
    `send`, `me`, `bin`, `select`, `leave`, with `@group` to address a group
    explicitly. A conference with no peers yet *refuses* the send with the real
    reason (`NO_CONNECTION`) rather than pretending to queue it: conference
    messages are live only.
- Tests: `tox_core_transport_test::test_group_lifecycle_over_the_tox_transport`
  (create → list → send → refuse an unknown group → leave, over real toxcore and
  a real `CoreService`), `tox::tests::test_conference_lifecycle_without_a_dht` and
  `test_conference_input_is_validated`, `core::tests::test_group_change_is_folded_into_state`,
  `test_group_message_is_published_counted_and_persisted`,
  `test_binary_group_message_is_not_undecodable`,
  `test_non_utf8_group_text_is_not_published`, `test_group_selector_resolution`,
  `database::tests::test_group_message_round_trips_through_the_database`,
  `core_service_test::test_groups_are_reported_as_unsupported_on_the_tcp_transport`,
  `ipc::protocol::tests::test_group_vocabulary_round_trips`,
  `commands::tests::test_parse_group_command` and the `/group` lines in
  `repl_commands_test::test_run_executes_all_repl_commands`.
- **`tests/message_exchange_test.rs`: core-to-core communication tests.** Two
  real `CoreService` actors over the TCP transport, asserting on what the other
  side receives: attribution to the sender, arrival order across a burst,
  byte-exact round trips of emoji / control characters / a body just under
  `max_message_length`, both directions on one connection, a repeated `connect`
  staying non-fatal, and the offline case — a message addressed to a peer that is
  not connected is reported as `queued`, shows up in `/info` as waiting, and is
  delivered once the peer appears.
- **Transport abstraction (`transport::CoreTransport`).** The core actor drives
  either the TCP transport or Tox through one narrow surface (lifecycle,
  identity, peer management, delivery, friend requests), so no front-end names a
  transport and both share the same bounded `AppEvent` inbox, the same
  decrypted-payload contract and the same error vocabulary.
- **Tox as a first-class core transport (`transport::ToxTransport`,
  `tox-protocol`).** A `spawn_blocking` bridge owns the toxcore instance, seeds
  the friend table and translates `ToxEvent`s into `AppEvent`s; every blocking
  `ToxSender` call is issued off the actor task. `CoreTransport::provides_encryption`
  is `true` for Tox, so the metaText envelope is not layered on top of toxcore's
  own peer-to-peer encryption.
- **Raw-byte Tox messages.** `ToxEvent::Message` now carries `body: Vec<u8>` and
  `ToxSender::send_message_bytes` sends an opaque payload, so a binary envelope
  survives; `ToxEvent::text` decodes strictly when a human-readable body is
  wanted. The old lossy `String::from_utf8_lossy` is gone.
- **`--bootstrap host:port:PUBLIC_KEY` for Tox** (`BootstrapNode::parse`,
  `CliArgs::is_valid_tox_bootstrap`): a private DHT node can be dialled without
  rebuilding, brackets support IPv6, and a malformed triple is rejected on the
  command line. `--port` now pins the UDP port toxcore binds.
- **Friend requests over the protocol (v3).** `Request::PeerRequests` /
  `Request::AcceptPeerRequest`, `Reply::PeerRequests` / `Reply::PeerRequestAccepted`
  and `CoreEvent::PeerRequestReceived`; `SessionInfo` gained `transport`,
  `public_identity`, `pending_requests` and `supports_friend_requests`. The
  shared presenter renders `/requests` and `/accept <index|public key>` for both
  the CLI and the TUI, and a friend request is never accepted automatically.
- `utils::abbreviate` for `D5F0…0831`-style identifiers, with multi-byte safe
  truncation.
- `tests/tox_core_transport_test.rs`: identity, restart stability, transport
  aware address validation, friend-request registration and the empty
  friend-request vocabulary, all driven through the public core interface. Its
  opt-in `test_two_cores_exchange_a_message_over_tox` (`--ignored`) runs the whole
  friend request → accept → connect → message flow between two core services on
  the public Tox DHT.

### Changed

- **The crate is now a Cargo workspace, so the layering of
  `docs/ARCHITECTURE.md` §2.1 is enforced by the compiler instead of by review.**
  The single package became a root binary package plus five member crates:
  `meta-text-proto` (the versioned request/reply/event vocabulary, framing,
  boundary validation and the shared `error`/`types`/`utils` — a leaf that cannot
  reach anything), `meta-text-backend` (crypto, database, the TCP and Tox
  transports, configuration, logging, command line vocabulary and `build.rs`),
  `meta-text-core` (`CoreService`/`CoreHandle`, the `CoreClient` contract and the
  TCP endpoint), `meta-text-tui` (presenter, wording, slash commands and the
  terminal rendering engine), `meta-text-cli` (the line oriented REPL) and
  `meta-text` itself (`src/main.rs` plus an umbrella lib that keeps the historical
  `meta_text::…` paths working for applications and for `tests/`). Because
  `meta-text-core` re-exports the configuration vocabulary but *not* `crypto`,
  `database`, `network` or `transport`, a front-end cannot name a key, a row or a
  socket: R3 is now a compile error. No public path changed and no behaviour
  changed; two review-era layering violations were fixed on the way (the
  endpoint's rate-limit defaults moved into `config`, which the endpoint now
  re-exports, and the protocol's hex key length became a wire constant of
  `meta-text-proto`), and Cargo features moved next to the code that needs them
  (`sqlite`/`postgres`/`mysql` with SQLx, `tox-protocol` with `libtoxcore`,
  `terminal-ui` with `crossterm`). Run `cargo test --workspace`: a bare
  `cargo test` would only execute the binary package's tests. Covered by the
  existing suite, which is unchanged and green in every feature combination;
  `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --workspace`
  (the CI documentation job) is clean again, including two intra-doc links to
  private items that the `workspace` run surfaced.
- **The core endpoint rate limit is now per client *address*, and its policy is
  configurable.** `ServerOptions::requests_per_second` / `request_burst` were only
  reachable from code, and the bucket lived on the connection — so a client that was
  shed could simply reconnect and start over. The budget is now keyed by the peer's
  address: a new socket from the same address inherits the spent allowance, while a
  different address gets its own (one client cannot starve the others). The table is
  bounded both ways a remote peer can push it — idle addresses expire after ten
  minutes, and a full table evicts the least recently seen address, so a new client
  is never refused. The policy now lives in the `[ipc]` section
  (`requests_per_second`, `request_burst`) with `--ipc-rate` / `--ipc-burst` as
  per-run overrides, `0` still disables the limit, and a headless start reports the
  limit it is applying. An older configuration file without `[ipc]` keeps working
  (the section is `#[serde(default)]`). Covered by four unit tests on the budget
  table, `ipc_socket_test::test_a_reconnect_does_not_refill_the_budget`, the
  updated flood test, and an end-to-end probe of the compiled binary.

- **`[logging] rotation_size_mb` is now enforced, and log retention is bounded by
  it.** The key was parsed, validated and then read by nothing, because
  `tracing-appender` rotates by time only — so an operator asking for 10 MB files
  got unlimited daily ones, which is exactly the "a key the binary ignores is a
  promise the config file cannot keep" failure A14/A42 set out to remove. A
  `logging::SizeRotatingWriter` (a plain `std::io::Write`, so it still composes with
  `tracing_appender::non_blocking`) now rolls a file over when it reaches the
  configured size *and* when the day changes: the day's first file keeps the
  familiar `logs/meta-text.log.2026-09-14` name, its continuations are `.001`,
  `.002`, … and `max_files` caps the total (active file included, oldest removed
  first). A write is never split across two files, a line larger than the whole
  limit is still written whole, a restart counts the bytes already on disk (instead
  of restarting the count at zero) and a missing `logs/` directory is created, as
  the previous appender did. Files in the directory that the appender did not write
  are never deleted. Covered by nine unit tests in `logging::tests`, and the
  startup line now reports the limit it is applying.
- **`--port 0` / `network.port = 0` is now accepted and means "let the OS pick a
  free port".** Both transports already behaved that way (the TCP listener binds
  an ephemeral port and `/peers` reports it; toxcore is passed `0` when no port is
  pinned), the `README` documented it for `/peers`, and every integration test
  starts from it — but both user-facing validators rejected it, so the documented
  case was unreachable from the command line. The bound port is shown by
  `/whoami` and `/peers`, which is what makes the value usable rather than a
  guess.
- **`[network] enable_ipv6` is now enforced, and `enable_upnp` no longer
  pretends.** Both keys used to parse and pass validation but be read by nothing,
  so the configuration file promised behaviour the binary did not have.
  `enable_ipv6` now binds a **dual-stack** IPv6 wildcard on the TCP listener (one
  `[::]` socket with `IPV6_V6ONLY` cleared through `socket2`, because `std` cannot
  set that option and a default IPv6 wildcard is v6-only on Windows; a host with no
  usable IPv6 stack falls back to the IPv4 wildcard), and it is passed to
  toxcore's `ipv6_enabled` on the Tox transport. `enable_upnp` is **not
  implemented** — no UPnP mapping client is linked — so `true` is rejected by
  `AppConfig::problems()` and the default/template ship `false`, rather than
  silently ignoring a request for a forwarded port.

- **`--transport tox` now runs the core actor, not a separate front-end.** The
  bespoke `ui::tox` REPL has been removed: `CoreService` owns a
  `transport::CoreTransport` (`Tcp(NetworkManager)` or `Tox(ToxTransport)`) and
  the transport is selected once, from the command line. Tox sessions therefore
  reuse the contact list, the message history, `/info`, `/history`, the REPL and
  the full screen TUI, and there is a single copy of every user-visible string.

- `--log-level` / `--debug` now really override `[logging] level`: the effective
  verbosity is resolved as `RUST_LOG` → explicit command line → configuration
  file → built-in default. `CliArgs::log_level` is now `Option<LogLevel>` so
  "no preference" can be distinguished from an explicit request.
- **`connected_peers` is documented as the transport's connection count, not as
  "peers you can message".** On TCP a socket is counted as soon as it is
  registered, which is one `hello` frame before the peer's nickname — the key a
  message is routed by — is known; `peer_nicknames` names the addressable subset
  and a send inside that window is buffered and flushed rather than lost. The
  core-to-core tests wait for the announced nickname instead of the socket count,
  which removes two intermittent `message_exchange_test` failures
  (`test_a_burst_of_messages_arrives_complete`, `test_binary_payload_round_trips`
  saw a `Queued` report where they asserted `Sent`).
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

- **No test writes into the source tree any more.** A doc example ran with the
  *package* directory as its working directory, so `AppConfig::load("config.toml")`
  and `ensure_directory("logs")` left a `config.toml` and a `logs/` directory
  behind in whichever crate root the example belonged to — which is why the
  repository's own `config.toml` kept showing up as modified. Both examples now
  use a `tempfile::tempdir()`, and `CONTRIBUTING.md` states the rule, because a
  test that dirties the working tree can hide a real failure in the next run.
- **The TCP message-exchange test no longer fails under load.** `PATIENCE` in
  `tests/message_exchange_test.rs` was 15 s; once the suite runs every workspace
  crate's tests in parallel, a busy machine can take longer than that to schedule
  a loopback handshake, which surfaced as a spurious "timed out waiting for both
  peers to know each other by name". The deadline is 30 s and documents why.

- **A Tox friend's nickname is now learned even when it arrives after the
  connection callback, so `/peers`, message attribution and `/msg <nickname>`
  agree with what the peer is called.** toxcore raises
  `friend_connection_status` when a friend becomes reachable and `friend_name`
  for the name itself, and the two can cross in either order; only the first was
  registered, and the friend table was refreshed only by it. When the name
  arrived second the friend stayed nameless for the whole session: `/peers`
  reported the peer as "(+1 still completing the handshake)" forever, inbound
  messages were labelled with an abbreviated public key, and `/msg <nickname>`
  failed with "does not match a friend of this Tox instance" — reproducibly, on
  one side of a mutual add. `tox_callback_friend_name` is now registered, its
  event refreshes the table and announces the newly learned name (once, and only
  for a reachable friend, so a reconnect does not print a second "connected"
  line), and message attribution refreshes a still-unnamed friend before
  labelling the message. Covered by
  `transport::tests::test_a_late_nickname_is_reported_once`, which pins the
  "announce exactly once" rule in both event orders.
- **The opt-in DHT tests no longer run at the same time.** Each one boots two
  toxcore instances and waits on the public network, so running the three of them
  in parallel made them compete for DHT round-trips and
  `test_two_cores_exchange_a_message_over_tox` lost its 120 s budget for the friend
  connection ("Alice never saw Bob connect"). They take a shared async mutex now,
  instead of relying on `--test-threads=1` being remembered.
- **A Tox transport dropped without `shutdown()` no longer wedges teardown.** The
  bridge is a `spawn_blocking` task, and a tokio runtime waits for its blocking
  tasks when it is dropped — but the stop flag was set only by the orderly
  `shutdown()`, so a transport torn down on any other path (an actor dropped with
  its runtime, or a front-end/test that panicked before `shutdown().await`) left
  the bridge looping in `ToxClient::next_event()` and the runtime waiting forever:
  a hang, not an error, which is how a panicking opt-in DHT test used to wedge the
  whole test binary. `ToxTransport` now stops its bridge on `Drop` (the loop
  re-reads the flag every 200 ms and the savedata is still written on the way out),
  and `transport::tests::test_dropping_the_transport_stops_its_bridge` pins it
  under a bounded timeout so a regression fails instead of hanging.
- **`app.max_friends` now holds for accepting a friend request, not only for
  `/add`.** Accepting created a contact unchecked, so the documented friend-list
  limit could be exceeded through the one path that was forgotten. One
  `friend_limit_error()` is now consulted by both, and it runs *before* the
  transport call, so a refused accept leaves the request pending for when the user
  has made room.
- **A friend-request command on the wrong transport no longer answers the wrong
  question.** `/accept 1` (and `/reject 1`) resolved the index against the pending
  list first, so on TCP — where that list can never have anything — the answer was
  "no pending request #1" instead of "friend requests only exist on the Tox
  transport". The capability is checked first now, which is also what `show_requests`
  already did.
- **The README's command table was missing two commands that already existed.**
  `/requests` and `/accept` shipped with the friend-request vocabulary but were
  never listed, so the documented command set disagreed with `help_lines()` — which
  the end-to-end REPL test now pins for all three friend-request verbs.
- **The pending friend-request and group-invitation lists were unbounded.** A friend
  request only needs our Tox address to be produced, so anyone could add one entry
  per identity they generate and grow the process on their own schedule — the one
  kind of growth R7 ("resource use must be bounded") exists to prevent, and the
  same defect that was closed for the event queue. Both lists are now capped
  (`transport::PENDING_REQUESTS_CAPACITY` / `PENDING_INVITES_CAPACITY`, 128) with
  **drop-newest**, so a flood cannot evict the request a user is about to accept;
  a refused entry is counted rather than silent (`/metrics` gained
  `friend_requests_dropped` and `group_invites_dropped`, protocol v6), and a
  repeated request is not counted as a refusal.
- **A refused accept no longer loses the friend request.**
  `ToxTransport::accept_request` removed the request before calling toxcore, so a
  refusal (for example a full friend list) forced the peer to send it again — the
  same consume-before-success mistake fixed for group invitations. Two caches that
  only ever grew are also bounded now: a group's cached title is dropped when the
  group is left, and the conference-handshake set is pruned against toxcore's
  current conference list.
- **A blank group rename was refused by the transport instead of the boundary.**
  `validation::group_name` allows an empty value on purpose (a group can be
  *created* before its title is known), but toxcore rejects a zero-length title
  with `INVALID_LENGTH`, so `/group rename ""` surfaced as
  `could not rename the group: … INVALID_LENGTH` — a network failure a user
  cannot act on. The rename path now requires a name and rejects a blank one with
  a usable `invalid_request`.
- **The default-feature test suite passes again.** Two tests that read the
  history back — `database::tests::test_group_message_round_trips_through_the_database`
  and the persistence half of
  `ipc::core::tests::test_group_message_is_published_counted_and_persisted` — were
  not gated on the `sqlite` feature, although every sibling persistence test is.
  `cargo test` (the build CI runs first, and the only build a plain `cargo test`
  gets) therefore failed with `Persistence requires a build with the sqlite
  feature`. The driver-dependent assertions are gated; the event, counter and
  state assertions in the core test run in every build.
- **A group's `joined` flag was a constant.** `ToxClient::snapshot` reported every
  conference as `connected: true` while the reply that had just created one said
  `false`. `connected` is now derived instead of asserted: the conference handshake
  (recorded from `conference_connected`) **or** more than one participant — the
  callback alone would be wrong, since tox.h documents it as firing only "after
  joining [a conference] with `tox_conference_join`" and the DHT test confirmed the
  *creator* never receives it. A conference with nobody else in it therefore shows
  as `connecting`, which is exactly the state in which a send fails with
  `NO_CONNECTION`.
- **A failed group join no longer consumed the invitation.**
  `ToxTransport::join_group` removed the invitation *before* attempting the join, so
  a transient failure (`FAIL_SEND`) burned a token that had never been used and the
  peer had to invite again. The token is now spent by a join that **succeeded**;
  replay is still refused, so the single-use rule is intact.
- **A group rename is now visible immediately.** `tox_conference_title` was never
  registered, so a peer renaming a conference only became visible in the next
  `/group list`. The callback is bound (`ToxEvent::ConferenceTitleChanged`) and
  folded into the existing `GroupChanged` event, so a rename is delivered to every
  front-end as `group_changed` — no protocol change was needed because that event
  already carries the group name. The author's `peer_number` is discarded on
  purpose: toxcore reports `UINT32_MAX` when it does not know who renamed the
  group.
- **A group failure used to be reported as a network error** whatever its cause.
  Every group request now keeps the code of the underlying error and only prefixes
  the operation, so "this transport has no groups" is an `invalid_request` that
  names the remedy, and "no such group" is a `not_found` — instead of both looking
  like a connectivity problem. The capability check also runs *before* state
  lookup, so the first answer a TCP user gets is the real reason rather than a
  missing group.
- **Concurrent requests could be lost on both sides of the IPC socket.** The
  client pump and `serve_connection` both selected over `read_message` directly.
  That function reads a length prefix and then the body, so it is *not* cancel
  safe: when the other branch (an outgoing write on the client, an inbound event
  on the server) won the race, the partially read frame was dropped and the
  stream desynchronised, losing replies and eventually the connection. It was
  reproducible: `ipc_socket_test::test_many_concurrent_requests_are_all_answered`
  failed 3 times in 30 runs before the change and 0 times in 50 runs after. Both
  ends now read in their own task and the `select!` loops poll channels only,
  which are cancel safe.
- **Logging precedence.** A `[logging] level` value in `config.toml` used to
  shadow the `--log-level` and `--debug` flags, so a scripted session asking for
  `--log-level error` still emitted informational lines on `stdout`. The command
  line now wins over the configuration file (`RUST_LOG` still wins over both).
- The TUI footer and README document the input-editing keys, so the advertised
  shortcuts match what the interface actually accepts.
- **Graceful shutdown could lose the session snapshot.** `Request::Shutdown`
  was acknowledged *before* the actor ran its terminal cleanup, so `main`
  returned while `save_session` was still in flight; dropping the Tokio runtime
  then aborted the write. The result was an intermittently missing
  `metatext-session.json` (and a spurious `Failed to write …` warning). The
  reply is now sent only after cleanup has finished, so a caller that awaits
  shutdown knows the session is on disk and the transport/storage are closed.
- **`integration_test::test_core_service_initialization_end_to_end` was not
  hermetic.** It built the core with `CliArgs::default()`, so the session
  snapshot was read from and written to the developer's real per-user data
  directory. A leftover contact made `contacts.is_empty()` fail on the next run
  and the test wrote its throwaway `DID-INTEGRATION` contact back into that real
  directory. The test now pins `--data-dir` to its temporary directory.
- **`cargo doc` failed without `--all-features`.** `src/lib.rs` linked the
  feature-gated `tox` module and `src/tui.rs` linked `MetaTextError` without a
  path; both are unresolved when the module or the item is absent, so
  `RUSTDOCFLAGS="-D warnings" cargo doc` (default features) aborted. The first
  is now plain text and the second uses the full `crate::error::` path.
- **The IPC token comparison could accept a longer token.** `secret_eq` folded
  the length difference into a `u8`, so `expected.len() ^ provided.len()` was
  truncated. A token 256 bytes longer than the configured one, sharing its
  prefix, compared equal: `secret_eq("token", "token" + 256 bytes)` returned
  `true`, which is an authentication bypass on the socket endpoint. The length
  difference is now kept at full width and every byte up to the longer length is
  visited, so a length mismatch can never be folded away.
- **Configuration and session snapshots were written in place.** `AppConfig::save`
  and `CoreService::save_session` wrote straight to the destination, so a crash
  or a full disk in the middle of the write could leave a truncated
  `config.toml` / `metatext-session.json` behind. Both now write a sibling
  `.tmp` file and rename it over the target, which is atomic on one filesystem.
- **`ipc_socket_test::test_silent_client_is_dropped_by_the_handshake_timeout`
  was flaky under load.** It relied on the endpoint's fixed five second
  handshake timer finishing inside the test's own fifteen second budget, which
  a saturated machine can exceed. The handshake timeout is now a
  `ServerOptions` field (`DEFAULT_HANDSHAKE_TIMEOUT` remains the default), so
  the test sets 250 ms and asserts against a generous budget; the suite went
  from ~5 s to ~0.3 s and passes repeatedly under CPU load.

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
