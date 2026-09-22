/*!
 * protocol.rs
 *
 * Versioned interface control document (ICD) between the metaText user
 * interfaces (CLI, TUI) and the core application service.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - A single, serializable request/response/event vocabulary
 * - Explicit protocol version negotiation
 * - Transport independent: the same types are used in-process and over TCP
 * - No presentation logic: replies carry data, never pre-formatted text
 *
 * # Safety and determinism
 *
 * Every type in this module is a plain data structure with `serde` derives.
 * Nothing here performs I/O, allocates unbounded resources or panics, so a
 * front-end can be reasoned about (and fuzzed) without touching the backend.
 */

use serde::{Deserialize, Serialize};

use crate::error::MetaTextError;
pub use crate::types::{ContentType, MessageKind};

/// Wire protocol version.
///
/// Bumped whenever the shape of [`Request`], [`Reply`] or [`CoreEvent`]
/// changes in a way that is *not* backwards compatible. Additive changes use
/// `#[serde(default)]` fields or a new request/reply pair, so an older client
/// keeps working against a newer server; such changes still get a new version so
/// the negotiation is honest about what the server can serve.
///
/// History:
/// - `1` — initial request/reply/event vocabulary.
/// - `2` — added [`CoreEvent::MessageUndecodable`]; undecodable payloads are
///   reported instead of being replaced with U+FFFD.
/// - `3` — added the transport identity (`transport`, `public_identity`,
///   `pending_requests`, `supports_friend_requests`) to [`SessionInfo`] and the
///   friend-request vocabulary ([`Request::PeerRequests`],
///   [`Request::AcceptPeerRequest`], [`Reply::PeerRequests`],
///   [`Reply::PeerRequestAccepted`], [`CoreEvent::PeerRequestReceived`]).
/// - `4` — added [`MessageKind`]: [`Request::SendMessage`] and
///   [`CoreEvent::MessageReceived`] carry a `kind` that defaults to
///   [`MessageKind::Text`], so a version 3 client that never sets it keeps
///   working. Everything from [`MIN_SUPPORTED_PROTOCOL_VERSION`] to
///   [`PROTOCOL_VERSION`] is served.
/// - `5` — added group chat: [`Request::Groups`], [`Request::CreateGroup`],
///   [`Request::JoinGroup`], [`Request::InviteToGroup`],
///   [`Request::SendGroupMessage`], [`Request::LeaveGroup`], the matching replies,
///   and [`CoreEvent::GroupMessageReceived`] / [`CoreEvent::GroupChanged`] /
///   [`CoreEvent::GroupInviteReceived`]. Every *new event* is additive for an
///   older front-end (it decodes and ignores an unknown tag); a new *request* is
///   only sent by a newer client.
/// - `6` — added [`Request::GroupInvites`] / [`Reply::GroupInvites`], so a
///   front-end that attaches *after* a group invitation arrived can still list
///   (and therefore join) it (a broadcast event is only seen live, so the count in
///   [`SessionInfo::pending_group_invites`] previously had no counterpart a late
///   subscriber could read), [`Request::RenameGroup`] /
///   [`Reply::GroupRenamed`], because the group's name is conference-wide state
///   that every participant sees, [`Request::RejectPeerRequest`] /
///   [`Reply::PeerRequestRejected`] so a user can discard a stranger's request
///   instead of only being able to *accept* it, and two [`MetricsView`] counters
///   (`friend_requests_dropped`, `group_invites_dropped`) that make the bounded
///   request/invitation lists observable.
/// - `7` — added [`Request::DeclineGroupInvite`] / [`Reply::GroupInviteDeclined`],
///   the invitation-side twin of [`Request::RejectPeerRequest`]: a pending group
///   invitation could be listed and joined but never answered *no*, so the only
///   way to make room in the bounded list was to join a conference and leave it.
///   Additive in the same way as version 6 — a new request is only sent by a newer
///   client, and a new reply is only produced for that request.
/// - `8` — the identity half of A12: [`SessionInfo::identity_fingerprint`] and
///   [`SessionInfo::peer_identities`] expose the announced identities so a user can
///   compare fingerprints, and [`MetricsView::peer_identities_changed`] /
///   [`MetricsView::peer_identities_refused`] make a change under a known nickname
///   and a pin that could not be kept observable. Additive: every new field is
///   `#[serde(default)]`, so a version 3 front-end decodes the snapshot unchanged.
/// - `9` — [`CoreEvent::MessageUndecodable`] carries the **reason** a payload cannot be
///   shown and the group it arrived in, because the rule it reports was widened from one
///   check to two: a body that is not valid UTF-8, and a body that contains a control
///   character (a front-end prints a body, so `ESC` in one is a command to the reader's
///   terminal — the rule the send path has always applied, now applied to what arrives).
///   An existing event was *extended* rather than a new tag added: an unknown tag is a
///   decode error for every front-end built before it, while an unknown field is ignored,
///   so both new fields are `#[serde(default)]` and a version 3 front-end keeps working.
///   Protocol 9 also adds [`MetricsView::greetings_refused`], the count of TCP greetings
///   whose nickname could not be displayed (the label rule of
///   [`crate::ipc::validation::peer_label`], applied where a peer-chosen string is
///   adopted).
pub const PROTOCOL_VERSION: u32 = 9;

/// Oldest protocol version this build still serves.
///
/// Keeping a window means a front-end can be upgraded independently of the
/// core: an older CLI keeps working until the window moves. A version outside
/// `MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION` is rejected during the
/// handshake with `unsupported_protocol`.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 3;

/// Whether `version` is inside the supported window.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::ipc::protocol::{
///     is_supported_protocol, MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION,
/// };
///
/// assert!(is_supported_protocol(PROTOCOL_VERSION));
/// assert!(is_supported_protocol(MIN_SUPPORTED_PROTOCOL_VERSION));
/// assert!(!is_supported_protocol(MIN_SUPPORTED_PROTOCOL_VERSION - 1));
/// assert!(!is_supported_protocol(PROTOCOL_VERSION + 1));
/// ```
#[must_use]
pub const fn is_supported_protocol(version: u32) -> bool {
    version >= MIN_SUPPORTED_PROTOCOL_VERSION && version <= PROTOCOL_VERSION
}

/// Longest request frame accepted on the wire, in bytes.
///
/// The length prefix is attacker controlled, so it is validated against this
/// bound *before* any buffer is allocated.
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;

/// Maximum nickname length, in `char`s.
///
/// Bounded so a front-end cannot inject an unbounded string into every log
/// record, peer handshake and TUI frame.
pub const MAX_NICKNAME_LEN: usize = 64;

/// Maximum status message length, in `char`s.
pub const MAX_STATUS_LEN: usize = 256;

/// Maximum contact identifier length, in `char`s.
pub const MAX_IDENTIFIER_LEN: usize = 128;

/// Maximum contact note length, in `char`s.
pub const MAX_NOTE_LEN: usize = 256;

/// Number of records a history request asks for when it names no limit.
///
/// The core answers `Request::History { limit: None }` with this many records, unless
/// the session was started with `--history-limit`, which replaces it for that run. It is
/// the same number `/history` documents for an argument-less call, so the command and an
/// embedder that sends no limit agree.
pub const DEFAULT_HISTORY_LIMIT: usize = 20;

/// Largest number of records one history request may ask for.
///
/// A larger limit is clamped rather than refused: the wire carries a `usize`, so an
/// oversized page is a sizing mistake rather than a protocol error — but the bound keeps
/// one request from making the core materialise an unbounded reply.
pub const MAX_HISTORY_LIMIT: usize = 1000;

/// Control characters a message body may contain.
///
/// Newlines and tabs are legitimate in chat text; every other control
/// character (NUL, escape, ...) is rejected so a front-end cannot smuggle
/// terminal control sequences into another user's screen.
pub const ALLOWED_MESSAGE_CONTROLS: [char; 2] = ['\n', '\t'];

/// Whether `value` contains a control character that is not explicitly allowed.
///
/// # Arguments
///
/// * `value` - Text to inspect.
/// * `allowed` - Control characters that are acceptable in this field.
///
/// # Examples
///
/// ```rust
/// use meta_text_proto::ipc::protocol::contains_forbidden_control;
///
/// assert!(contains_forbidden_control("a\u{0}b", &[]));
/// assert!(!contains_forbidden_control("a\nb", &['\n']));
/// ```
#[must_use]
pub fn contains_forbidden_control(value: &str, allowed: &[char]) -> bool {
    value
        .chars()
        .any(|character| character.is_control() && !allowed.contains(&character))
}

/// Stable, machine readable failure categories.
///
/// Front-ends must switch on this code instead of matching error strings so
/// that wording can change without breaking clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request was structurally valid but semantically wrong.
    InvalidRequest,
    /// The referenced entity (contact, conversation, peer) does not exist.
    NotFound,
    /// The transport could not complete the operation.
    Network,
    /// A cryptographic primitive failed or rejected its input.
    Cryptography,
    /// The persistence backend is unavailable or failed.
    Database,
    /// The configuration is invalid or missing.
    Configuration,
    /// The client presented a bad or missing authentication token.
    Unauthorized,
    /// The protocol version is not supported by this server.
    UnsupportedProtocol,
    /// The service is saturated and shed the request (backpressure).
    Backpressure,
    /// The request took longer than the caller's deadline.
    Timeout,
    /// An unexpected internal fault; the service stays alive.
    Internal,
}

impl ErrorCode {
    /// Whether a failure of this class must abort the session.
    ///
    /// Only unrecoverable faults are critical; everything else is reported to
    /// the user and the session continues (fail-safe behaviour).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::ipc::protocol::ErrorCode;
    ///
    /// assert!(!ErrorCode::NotFound.is_critical());
    /// assert!(ErrorCode::Internal.is_critical());
    /// ```
    #[must_use]
    pub const fn is_critical(self) -> bool {
        matches!(self, Self::Internal)
    }
}

/// How badly a failure affects the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Informational; nothing failed.
    Info,
    /// The operation failed but the session is healthy.
    Recoverable,
    /// The session cannot continue reliably.
    Critical,
}

/// A transport-safe error description.
///
/// `MetaTextError` contains boxed source errors which cannot be serialized, so
/// the core flattens it into this value at the boundary. The original error is
/// still logged inside the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    /// Machine readable category.
    pub code: ErrorCode,

    /// Impact on the session.
    pub severity: Severity,

    /// Human readable, non-localised explanation.
    pub message: String,
}

impl ErrorInfo {
    /// Build a recoverable error with the given code.
    ///
    /// # Arguments
    ///
    /// * `code` - Machine readable category.
    /// * `message` - Human readable explanation.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::ipc::protocol::{ErrorCode, ErrorInfo};
    ///
    /// let error = ErrorInfo::new(ErrorCode::NotFound, "no such friend");
    /// assert_eq!(error.code, ErrorCode::NotFound);
    /// ```
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: if code.is_critical() {
                Severity::Critical
            } else {
                Severity::Recoverable
            },
            message: message.into(),
        }
    }

    /// Build a critical error, used for faults that must end the session.
    #[must_use]
    pub fn critical(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: Severity::Critical,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ErrorInfo {}

impl From<MetaTextError> for ErrorInfo {
    /// Flatten a rich internal error into a serializable description.
    fn from(error: MetaTextError) -> Self {
        // Errors that only affect one operation stay recoverable; resource and
        // internal faults are escalated because the session may be unsafe.
        match error {
            MetaTextError::Configuration { message, .. } => {
                Self::new(ErrorCode::Configuration, message)
            }
            MetaTextError::Network { message, .. } => Self::new(ErrorCode::Network, message),
            MetaTextError::Cryptographic { message, .. } => {
                Self::new(ErrorCode::Cryptography, message)
            }
            MetaTextError::Database { message, .. } => Self::new(ErrorCode::Database, message),
            MetaTextError::UserInterface { message, .. } => Self::new(ErrorCode::Internal, message),
            MetaTextError::Message { message, .. } => Self::new(ErrorCode::InvalidRequest, message),
            MetaTextError::Authentication { message, .. } => {
                Self::new(ErrorCode::Unauthorized, message)
            }
            MetaTextError::Validation { message, .. } => {
                Self::new(ErrorCode::InvalidRequest, message)
            }
            MetaTextError::Resource { message, .. } => {
                Self::critical(ErrorCode::Backpressure, message)
            }
            MetaTextError::Internal { message, .. } => Self::critical(ErrorCode::Internal, message),
        }
    }
}

/// A single operation requested by a front-end.
///
/// Serialized as an internally tagged object (`{"type": "...", ...}`) so that
/// unknown request kinds fail fast with a serde error instead of being
/// silently misinterpreted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Liveness probe; echoes `echo` back unchanged.
    Ping {
        /// Opaque value returned verbatim in [`Reply::Pong`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        echo: Option<String>,
    },

    /// Full session snapshot used by `/info` and the TUI header.
    SessionInfo,

    /// Runtime counters used by `/stats`.
    Statistics,

    /// The friend list, in insertion order.
    ListContacts,

    /// Add a friend by DID-like identifier.
    AddContact {
        /// Opaque contact identifier (stored as the placeholder public key).
        identifier: String,
        /// Optional human readable note.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },

    /// Remove a friend by 1-based index or case-insensitive name fragment.
    RemoveContact {
        /// 1-based index or name fragment.
        target: String,
    },

    /// Change (or clear) the local nickname.
    SetNickname {
        /// New nickname; an empty string only reports the current one.
        nickname: String,
    },

    /// Change (or clear) the personal status message.
    SetStatus {
        /// New status text; an empty string only reports the current one.
        text: String,
    },

    /// Select the active conversation by 1-based index or name fragment.
    SelectConversation {
        /// 1-based index or name fragment; empty queries the current chat.
        target: String,
    },

    /// Dial a peer address (`host:port`) and keep it connected.
    Connect {
        /// Remote address to dial.
        address: String,
    },

    /// Encrypt and route a chat message.
    SendMessage {
        /// Explicit destination nickname; `None` uses the active conversation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        /// Message body: UTF-8 text, or lowercase hexadecimal when
        /// `content_type` is `binary`.
        text: String,
        /// Ordinary message or third-person action; version 3 clients omit it.
        #[serde(default)]
        kind: MessageKind,
        /// How to interpret `text`; version 3 clients omit it and mean `text`.
        #[serde(default)]
        content_type: ContentType,
    },

    /// The most recent persisted messages.
    History {
        /// Maximum number of records; `None` uses [`DEFAULT_HISTORY_LIMIT`], or the
        /// session's `--history-limit` when it was started with one. A value above
        /// [`MAX_HISTORY_LIMIT`] is clamped to it rather than refused.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },

    /// Persist nickname, contacts and statistics to disk.
    SaveSession,

    /// Friend requests waiting for an answer.
    PeerRequests,

    /// Accept a pending friend request by public key.
    AcceptPeerRequest {
        /// The requester's 64 character public key.
        public_key: String,
    },

    /// Reject a pending friend request.
    ///
    /// Rejecting is a local decision: the pending list is bounded
    /// (`transport::PENDING_REQUESTS_CAPACITY`), so without a way to discard an
    /// entry the only way to make room would be to accept strangers. Nothing is
    /// sent to the requester — a friend request becomes a friendship only when it
    /// is accepted.
    RejectPeerRequest {
        /// The requester's 64 character public key.
        public_key: String,
    },

    /// Group chats and whether this transport can host them.
    Groups,

    /// Group invitations waiting for an answer.
    ///
    /// The counterpart of [`CoreEvent::GroupInviteReceived`]: that event is
    /// broadcast, so it is only seen by a front-end that was already attached.
    /// A front-end that attaches later (or that missed the event) reads the
    /// pending invitations — and the tokens needed to join them — here, the same
    /// way [`Request::PeerRequests`] serves friend requests.
    GroupInvites,

    /// Discard a pending group invitation.
    ///
    /// The counterpart of [`Request::JoinGroup`], and the invitation-side twin of
    /// [`Request::RejectPeerRequest`]: the invitation list is bounded
    /// (`transport::PENDING_INVITES_CAPACITY`), so without a way to say *no* the
    /// only way to make room was to join a conference and leave it again. Nothing
    /// is sent to the inviter — an invitation is a capability handed to us, not a
    /// relationship — so discarding one is a purely local decision.
    DeclineGroupInvite {
        /// Single-use token from [`Request::GroupInvites`] (the same value
        /// [`Request::JoinGroup`] accepts).
        token: String,
    },

    /// Create a group and join it.
    CreateGroup {
        /// Group name (the conference title; may be empty).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },

    /// Join a group from an invitation token.
    JoinGroup {
        /// Single-use token from [`CoreEvent::GroupInviteReceived`].
        token: String,
    },

    /// Rename a group.
    ///
    /// The name belongs to the group, not to this session: every participant sees
    /// it (the transport forwards it and reports it as
    /// [`CoreEvent::GroupChanged`]), so this is a mutation of shared state, not a
    /// local label.
    RenameGroup {
        /// Group to rename; `None` uses the active group.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_id: Option<String>,
        /// The new group name (the conference title).
        name: String,
    },

    /// Invite a peer to a group.
    InviteToGroup {
        /// Group to invite them to.
        group_id: String,
        /// Peer nickname to invite.
        peer: String,
    },

    /// Send a message to a group.
    SendGroupMessage {
        /// Group to send to; `None` uses the active group.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_id: Option<String>,
        /// Message body: UTF-8 text, or lowercase hexadecimal when
        /// `content_type` is `binary`.
        text: String,
        /// Ordinary message or third-person action.
        #[serde(default)]
        kind: MessageKind,
        /// How to interpret `text`.
        #[serde(default)]
        content_type: ContentType,
    },

    /// Leave a group.
    LeaveGroup {
        /// Group to leave; `None` uses the active group.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_id: Option<String>,
    },

    /// Operational counters for monitoring.
    ///
    /// A superset of [`Request::Statistics`]: the same traffic counters plus the
    /// queue depths, shed counts and subscriber counts an operator needs to tell
    /// "quiet" from "stuck".
    Metrics,

    /// Stop the core service after replying.
    Shutdown,
}

/// Data returned for a successful [`Request`].
///
/// Replies are pure data; localisation and layout stay in the front-end.
///
/// The largest variant ([`Reply::Metrics`]) makes the enum about 520 bytes. It is
/// deliberately not boxed: a reply is built, sent and dropped one at a time, so
/// the move is a `memcpy` of a few hundred bytes, while a `Box` would add an
/// allocation to every reply and an indirection to every reader — a cost paid by
/// the common case to shrink a value that is never stored in bulk. The one place
/// where boxing is worth it is already boxed: the handshake snapshot in
/// [`ServerMessage::Welcome`], which a server holds and clones per client.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// Answer to [`Request::Ping`].
    Pong {
        /// The value supplied with the ping.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        echo: Option<String>,
    },

    /// Answer to [`Request::SessionInfo`].
    Session {
        /// Session snapshot.
        session: SessionInfo,
    },

    /// Answer to [`Request::Statistics`].
    Statistics {
        /// Runtime counters.
        statistics: StatisticsView,
    },

    /// Answer to [`Request::ListContacts`].
    Contacts {
        /// Contacts in insertion order.
        contacts: Vec<ContactView>,
    },

    /// A friend was added.
    ContactAdded {
        /// 1-based position in the friend list.
        index: usize,
        /// Identifier that was added.
        identifier: String,
    },

    /// A friend was already present; nothing changed.
    ContactExists {
        /// Identifier that matched.
        identifier: String,
    },

    /// A friend was removed.
    ContactRemoved {
        /// 1-based position the friend occupied.
        index: usize,
        /// Name of the removed friend.
        name: String,
    },

    /// Answer to [`Request::SelectConversation`].
    Conversation {
        /// Name of the active conversation, if one is selected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active: Option<String>,
        /// 1-based position of the active conversation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },

    /// Answer to [`Request::SaveSession`].
    Saved {
        /// Path the session was written to.
        path: String,
    },

    /// Answer to [`Request::History`].
    History {
        /// Whether durable persistence is available at all.
        persistent: bool,
        /// Stored messages, newest first.
        messages: Vec<MessageView>,
    },

    /// A mutation that changed server state.
    Updated {
        /// Short machine readable subject, e.g. `"nickname"`.
        subject: String,
        /// Human readable confirmation detail.
        detail: String,
    },

    /// Answer to [`Request::SendMessage`].
    Sent {
        /// Resolved destination nickname.
        target: String,
        /// Delivery report.
        report: SendReport,
    },

    /// Answer to [`Request::Shutdown`]; the service stops right after.
    ShuttingDown {
        /// Always `true`; present so the reply is a struct variant.
        #[serde(default)]
        acknowledged: bool,
    },

    /// Answer to [`Request::PeerRequests`].
    PeerRequests {
        /// Pending requests, oldest first.
        requests: Vec<PeerRequestView>,
    },
    /// Answer to [`Request::AcceptPeerRequest`].
    PeerRequestAccepted {
        /// Public key that was accepted.
        public_key: String,
    },
    /// Answer to [`Request::RejectPeerRequest`].
    PeerRequestRejected {
        /// Public key that was discarded.
        public_key: String,
    },

    /// Answer to [`Request::Groups`].
    ///
    /// Succeeds even on a transport without groups, with `supported: false` and
    /// an empty list, so a front-end can explain the situation instead of showing
    /// a failure.
    Groups {
        /// Whether the active transport can host group chats at all.
        supported: bool,
        /// Groups this session is in.
        groups: Vec<GroupView>,
    },

    /// Answer to [`Request::GroupInvites`].
    ///
    /// Succeeds on every transport, like [`Reply::Groups`]: a transport without
    /// groups reports `supported: false` and an empty list rather than failing, so
    /// a front-end can say *why* there is nothing to show.
    GroupInvites {
        /// Whether the active transport can host group chats at all.
        supported: bool,
        /// Invitations waiting for an answer, oldest first.
        invites: Vec<GroupInviteView>,
    },

    /// Answer to [`Request::CreateGroup`].
    GroupCreated {
        /// The group that was created.
        group: GroupView,
    },

    /// Answer to [`Request::JoinGroup`].
    GroupJoined {
        /// The group that was joined.
        group: GroupView,
    },

    /// Answer to [`Request::DeclineGroupInvite`].
    ///
    /// The token is echoed so a front-end can report *which* invitation was
    /// discarded without keeping its own copy of the list.
    GroupInviteDeclined {
        /// Token that was discarded (lowercase hexadecimal).
        token: String,
    },

    /// Answer to [`Request::RenameGroup`].
    GroupRenamed {
        /// Group that was renamed.
        group_id: String,
        /// The name it now has.
        group: String,
    },

    /// Answer to [`Request::InviteToGroup`].
    GroupInvited {
        /// Group the peer was invited to.
        group_id: String,
        /// Peer that was invited.
        peer: String,
    },

    /// Answer to [`Request::SendGroupMessage`].
    GroupSent {
        /// Group the message was sent to.
        group_id: String,
        /// Group name at the time of the send.
        group: String,
        /// Payload length as it crossed the transport.
        wire_bytes: u64,
    },

    /// Answer to [`Request::LeaveGroup`].
    GroupLeft {
        /// Group that was left.
        group_id: String,
    },

    /// Answer to [`Request::Metrics`].
    Metrics {
        /// Operational snapshot.
        metrics: MetricsView,
    },
}

/// A friend request waiting for an answer, flattened for the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRequestView {
    /// The requester's 64 character public key.
    pub public_key: String,
    /// The message attached to the request (may be empty).
    pub message: String,
    /// Abbreviated key for display (`D5F0…0831`).
    pub short_key: String,
}

/// A group chat, flattened for the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupView {
    /// Stable group identifier (the transport's, not a local one).
    pub id: String,
    /// Group name (empty when the transport has not told us yet).
    pub name: String,
    /// How many peers are online.
    pub members: usize,
    /// Whether this session has completed the group handshake.
    pub joined: bool,
    /// Abbreviated identifier for display (`D5F0…0831`).
    pub short_id: String,
}

/// A group invitation waiting for an answer, flattened for the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInviteView {
    /// Inviting peer's nickname (empty until announced).
    pub peer: String,
    /// Inviting peer's identifier.
    pub peer_id: String,
    /// Single-use token to pass to [`Request::JoinGroup`].
    pub token: String,
}

/// Outcome of a send attempt, flattened for the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendOutcomeKind {
    /// Handed to at least one connected peer.
    Sent,
    /// Buffered because the destination is offline.
    Queued,
    /// Rejected because the destination outbox is full.
    Dropped,
}

/// Delivery report attached to [`Reply::Sent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendReport {
    /// What the transport did with the message.
    pub outcome: SendOutcomeKind,
    /// Message id echoed back by acknowledgements, when one was assigned.
    pub message_id: Option<u64>,
    /// Number of peers the ciphertext was queued for.
    pub queued_for: usize,
    /// One-based position in the destination outbox (queued messages only).
    pub queue_position: Option<usize>,
    /// Ciphertext length in bytes.
    pub wire_bytes: u64,
    /// Short hex preview of the ciphertext, for operator confidence only.
    pub ciphertext_preview: String,
    /// Whether encryption was enabled for this message.
    pub encrypted: bool,
    /// Whether the message was mirrored into durable storage.
    pub persisted: bool,
}

/// Session level snapshot consumed by `/info`, `/whoami` and the TUI header.
///
/// The `supports_*` fields are capability bits, not independent options: a
/// front-end asks "can this core do X before I offer it", and each answer is
/// genuinely two-valued. An enum per capability would triple the field count and
/// turn "this core predates the question" (which today reports `false` through
/// `#[serde(default)]`) into a decode failure, which is exactly what the
/// compatibility window exists to avoid.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Local nickname announced to peers.
    pub nickname: String,
    /// Local status message.
    pub status_message: String,
    /// Announced identity of this session (never the encryption key).
    ///
    /// The public half of the instance's persistent key pair, 64 hexadecimal
    /// characters; the secret half never leaves the core. Stable across restarts of
    /// the same data directory, which is what lets a peer pin it.
    pub identity: String,
    /// Requested application mode (`CLI`, `TUI`, ...).
    pub mode: String,
    /// Application version.
    pub version: String,
    /// Active encryption algorithm, or `"disabled"`.
    pub encryption: String,
    /// Whether encryption is enabled.
    pub encryption_enabled: bool,
    /// Whether the transport listener is running.
    pub network_running: bool,
    /// Configured transport port.
    pub network_port: u16,
    /// Number of configured bootstrap nodes.
    pub network_bootstrap_nodes: usize,
    /// Connected peer count.
    ///
    /// On TCP this counts a registered connection, which can be a moment before
    /// the peer's nickname (`hello` frame) has been read; `peer_nicknames` holds
    /// the subset that has identified itself. A peer is addressable by name —
    /// and therefore usable as a message target — only once it is listed there.
    pub connected_peers: usize,
    /// Desired-but-unreachable peer count.
    pub pending_peers: usize,
    /// Maximum accepted inbound connections.
    pub max_connections: u32,
    /// Buffered messages waiting for a peer.
    pub queued_messages: usize,
    /// Addresses being retried in the background.
    pub desired_peers: Vec<String>,
    /// Nicknames of connected peers.
    pub peer_nicknames: Vec<String>,
    /// Bound listener address, if any.
    pub local_address: Option<String>,
    /// Whether the persistence backend is connected.
    pub database_connected: bool,
    /// Database connection string.
    pub database: String,
    /// Whether the backend can actually store data.
    pub persistent: bool,
    /// Configured maximum database connections.
    pub database_max_connections: u32,
    /// Number of known contacts.
    pub friend_count: usize,
    /// Configured maximum number of contacts.
    pub max_friends: usize,
    /// Name of the active conversation, when one is selected.
    pub active_chat: Option<String>,
    /// Uptime in whole seconds.
    pub uptime_seconds: i64,
    /// Configuration file path.
    pub config_path: String,
    /// Data directory path.
    pub data_dir: String,
    /// Name of the application as configured.
    pub app_name: String,
    /// Version of the application as configured.
    pub app_version: String,
    /// Configured maximum message length in characters.
    pub max_message_length: usize,
    /// Configured session auto-save interval in seconds.
    pub auto_save_interval: u64,
    /// Active transport name (`"tcp"` or `"tox"`).
    pub transport: String,
    /// Transport supplied cryptographic identity, when it has one.
    ///
    /// `Some` for Tox (the 76 character address a peer has to add), `None` for
    /// TCP, which has no identity of its own.
    pub public_identity: Option<String>,
    /// Friend requests waiting for an answer (Tox only, `0` for TCP).
    pub pending_requests: usize,
    /// Whether the transport can produce friend requests at all.
    pub supports_friend_requests: bool,
    /// Groups this session is in (`0` for TCP).
    #[serde(default)]
    pub groups: usize,
    /// Group invitations waiting for an answer (`0` for TCP).
    #[serde(default)]
    pub pending_group_invites: usize,
    /// Whether the transport can host group chats at all.
    #[serde(default)]
    pub supports_groups: bool,
    /// Fingerprint of this session's identity, in groups of four.
    ///
    /// What a user reads out to a peer so the two can compare identities out of
    /// band. It is a short form of [`Self::identity`], not a second identity: an
    /// empty value means this core predates the field.
    #[serde(default)]
    pub identity_fingerprint: String,
    /// What is known about the identities of peers that have been seen.
    ///
    /// The pinned table, sorted by nickname: the identity each peer announced the
    /// first time that nickname was seen, or the one it announced after a change.
    /// It is *not* the same set as [`Self::peer_nicknames`] — a pinned peer may be
    /// offline right now — and a change is flagged per entry rather than refused.
    /// Empty for a core that predates the field, and for a transport that carries no
    /// identity frames (a Tox build identifies a peer by its own address).
    #[serde(default)]
    pub peer_identities: Vec<PeerIdentityView>,
}

/// The identity one connected peer announced.
///
/// The identity is *announced*, not authenticated: a nickname is self-asserted and
/// any holder of the session passphrase can claim one, so this describes what a
/// peer said about itself, not who it is. The pin is what turns "it said" into
/// "and it said the same thing last time".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerIdentityView {
    /// The nickname the peer announced.
    pub nickname: String,

    /// The identity it announced, 64 hexadecimal characters.
    pub identity: String,

    /// Fingerprint of [`Self::identity`], in groups of four.
    pub fingerprint: String,

    /// Whether the identity differs from the one pinned for this nickname before
    /// this run.
    ///
    /// A change is *reported*, not refused (see the architecture document's A12):
    /// the value travels in the clear, so refusing would not stop the peer — but a
    /// user can see that the peer behind a name is not the one that was there last
    /// time.
    #[serde(default)]
    pub changed: bool,
}

/// Runtime counters consumed by `/stats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatisticsView {
    /// Messages sent since the session started.
    pub messages_sent: u64,
    /// Messages received since the session started.
    pub messages_received: u64,
    /// Live connection count.
    pub active_connections: u32,
    /// Bytes written to the transport.
    pub bytes_sent: u64,
    /// Bytes read from the transport.
    pub bytes_received: u64,
    /// Session uptime in whole seconds.
    pub uptime_seconds: i64,
}

/// Operational snapshot consumed by `/metrics`.
///
/// Everything a monitor needs to distinguish "idle" from "stuck": the same
/// traffic counters as [`StatisticsView`], plus how much work is queued, how much
/// was shed by a bounded buffer, and how many front-ends are attached. Every
/// field is a value the core can read without blocking, so `/metrics` is cheap
/// enough to poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsView {
    /// Session uptime in whole seconds.
    pub uptime_seconds: i64,

    /// Active transport name (`"tcp"` or `"tox"`).
    pub transport: String,

    /// Peers the transport currently holds a connection to.
    ///
    /// This is the transport's own count, so on TCP it includes a socket whose
    /// peer has not yet announced its nickname (that window is one `hello` frame
    /// wide and a send inside it is buffered, not lost). Use
    /// [`SessionInfo::peer_nicknames`] when the question is how many peers are
    /// addressable.
    pub peers_connected: usize,

    /// Known but unreachable peers.
    pub peers_pending: usize,

    /// Payloads buffered for an unreachable peer.
    pub payloads_queued: usize,

    /// Payloads dropped because they waited longer than the outbox TTL.
    pub payloads_expired: u64,

    /// Payloads shed by a bounded buffer instead of being queued (a peer
    /// connection's outbound queue on TCP, the toxcore callback hand-off on Tox).
    pub payloads_dropped: u64,

    /// Known friends.
    pub friend_count: usize,

    /// Configured friend limit.
    pub max_friends: usize,
    /// Groups this session is in.
    pub groups: usize,
    /// Whether the active transport can host group chats.
    pub groups_supported: bool,
    /// Friend requests waiting for an answer.
    pub friend_requests_pending: usize,

    /// Friend requests refused because that list was full.
    ///
    /// A request needs only our Tox address to be produced, so the pending list is
    /// bounded; a refusal is counted here rather than being invisible. Non-zero
    /// means somebody sent more requests than could wait at once.
    #[serde(default)]
    pub friend_requests_dropped: u64,

    /// Group invitations refused because that list was full.
    #[serde(default)]
    pub group_invites_dropped: u64,

    /// Requests waiting to be executed by the actor right now.
    pub request_queue_depth: usize,

    /// Capacity of that queue; `depth == capacity` means a front-end is about to
    /// be told to slow down.
    pub request_queue_capacity: usize,

    /// Front-ends currently subscribed to the event stream.
    pub event_subscribers: usize,

    /// How long the most recently served request waited in the queue, in
    /// microseconds. This is the *actor lag* a monitor needs: it grows when the
    /// actor is busy with something else (a slow database write, a transport
    /// call) or when a front-end floods the queue.
    pub request_wait_last_us: u64,

    /// Worst queue wait observed since start, in microseconds.
    pub request_wait_max_us: u64,

    /// How long the most recent request took to execute, in microseconds.
    pub request_service_last_us: u64,

    /// Worst execution time observed since start, in microseconds.
    pub request_service_max_us: u64,

    /// Requests served since start.
    pub requests_served: u64,

    /// Messages sent since the session started.
    pub messages_sent: u64,

    /// Messages received since the session started.
    pub messages_received: u64,

    /// Bytes written to the transport.
    pub bytes_sent: u64,

    /// Bytes read from the transport.
    pub bytes_received: u64,

    /// Whether the persistence backend is connected.
    pub database_connected: bool,

    /// Whether persistence actually stores anything.
    pub persistent: bool,

    /// Peer identities that changed under a nickname already pinned.
    ///
    /// Non-zero means a peer that was seen before announced a different identity.
    /// The change is reported rather than refused — the identity travels in the
    /// clear and is not yet authenticated (A12) — so this counter and the warning
    /// it accompanies are what make it visible.
    #[serde(default)]
    pub peer_identities_changed: u64,

    /// Identity announcements that could not be pinned.
    ///
    /// Either the announcement was unusable (no nickname, or a key that is not 64
    /// hexadecimal characters) or the table of remembered identities is full. A
    /// refused pin cannot detect a later change, which is why it is counted rather
    /// than dropped silently.
    #[serde(default)]
    pub peer_identities_refused: u64,

    /// Greetings refused because the nickname they announced cannot be displayed.
    ///
    /// A peer chooses the nickname in a TCP greeting and a front-end prints it, so it is
    /// held to the label rule (1..=64 characters, no control character). A greeting that
    /// fails it is refused — the peer stays connected and keeps its frames, but it has no
    /// name — and counted here, because a peer that does it on purpose does it once per
    /// connection and a log line per attempt is not a signal an operator can see. Zero on
    /// the Tox transport, which has no greeting to refuse: its labels come from toxcore
    /// reads, where an unusable name is simply not a name.
    #[serde(default)]
    pub greetings_refused: u64,
}

/// One friend, flattened for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactView {
    /// Contact UUID.
    pub id: String,
    /// Display name / DID.
    pub name: String,
    /// Stable status name (`online`, `offline`, ...).
    pub status: String,
    /// Whether the status counts as online.
    pub online: bool,
    /// Optional note.
    pub note: Option<String>,
    /// Whether the contact is blocked.
    pub blocked: bool,
    /// Last seen timestamp, RFC 3339, if known.
    pub last_seen: Option<String>,
}

/// One persisted message, flattened for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageView {
    /// `"out"` for sent, `"in"` for received.
    pub direction: String,
    /// Conversation partner.
    pub peer: String,
    /// Message body.
    pub body: String,
    /// Ciphertext length in bytes.
    pub wire_bytes: u64,
    /// Ordinary message or third-person action.
    #[serde(default)]
    pub kind: MessageKind,
    /// How to interpret `body`; a `binary` body is lowercase hexadecimal.
    #[serde(default)]
    pub content_type: ContentType,
    /// Creation timestamp, RFC 3339.
    pub created_at: String,
}

/// An asynchronous notification pushed from the core to every front-end.
///
/// Events are broadcast (not queued per client), so a slow front-end may lag;
/// the transport surfaces that as a gap rather than blocking the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    /// The core finished starting and is ready to serve requests.
    Ready {
        /// Session snapshot at startup.
        session: Box<SessionInfo>,
    },
    /// An encrypted message was received and decrypted.
    MessageReceived {
        /// Nickname announced by the sender.
        peer: String,
        /// Remote address of the sender.
        peer_id: String,
        /// Decrypted body.
        body: String,
        /// Ciphertext length in bytes.
        wire_bytes: u64,
        /// Ordinary message or third-person action.
        ///
        /// Defaulted so a version 3 front-end, which does not know the field,
        /// still decodes the event.
        #[serde(default)]
        kind: MessageKind,
        /// How the body must be interpreted.
        ///
        /// Defaulted like `kind`: a version 3 front-end treats every body as text.
        /// When this is `binary` the `body` is lowercase hexadecimal.
        #[serde(default)]
        content_type: ContentType,
    },
    /// A peer acknowledged one of our outgoing messages.
    MessageDelivered {
        /// Nickname that acknowledged.
        peer: String,
        /// Remote address of that peer.
        peer_id: String,
        /// Identifier returned by the send.
        message_id: u64,
    },
    /// A frame was decrypted but its payload cannot be shown.
    ///
    /// Reported explicitly rather than being replaced with U+FFFD or printed as it
    /// arrived, so a front-end can tell the user that something arrived but was not
    /// displayed. Two payloads land here: one that is not valid UTF-8, and one that
    /// contains a control character the protocol does not allow in a body
    /// ([`ALLOWED_MESSAGE_CONTROLS`]) — a body is printed by a front-end, so `ESC` in one
    /// is a command to the reader's terminal. The frame is still acknowledged at the
    /// transport level, so the sender is not left retrying something that arrived.
    MessageUndecodable {
        /// Nickname announced by the sender.
        peer: String,
        /// Remote address of the sender.
        peer_id: String,
        /// Ciphertext length in bytes.
        wire_bytes: u64,
        /// Why the payload cannot be shown, as a phrase a sentence can use:
        /// `is not valid UTF-8 text` or `contains control characters`.
        #[serde(default)]
        reason: String,
        /// Identifier of the group it was sent to, absent for a direct message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_id: Option<String>,
        /// Name of that group, absent for a direct message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
    },
    /// A peer completed the handshake.
    PeerConnected {
        /// Remote address.
        peer_id: String,
        /// Nickname announced, if already known.
        nickname: String,
    },
    /// A peer disconnected.
    PeerDisconnected {
        /// Remote address.
        peer_id: String,
        /// Reason reported by the transport.
        reason: String,
    },
    /// The active conversation changed.
    ConversationChanged {
        /// New destination nickname.
        target: String,
    },
    /// The local nickname changed.
    NicknameChanged {
        /// New nickname.
        nickname: String,
    },
    /// The status message changed.
    StatusChanged {
        /// New status text.
        text: String,
    },
    /// A peer asked to become a friend (Tox only).
    ///
    /// The request is never accepted automatically: a front-end has to name the
    /// public key with [`Request::AcceptPeerRequest`].
    PeerRequestReceived {
        /// The requester's 64 character public key.
        public_key: String,
        /// Abbreviated key for display (`D5F0…0831`).
        short_key: String,
        /// The message attached to the request (may be empty).
        message: String,
    },
    /// A message arrived in a group.
    GroupMessageReceived {
        /// Stable group identifier.
        group_id: String,
        /// Group name (empty when unknown).
        #[serde(default)]
        group: String,
        /// Sender's name inside the group.
        peer: String,
        /// Sender's address inside the group.
        #[serde(default)]
        peer_id: String,
        /// Wire length of the payload in bytes.
        wire_bytes: u64,
        /// Message body; lowercase hexadecimal when `content_type` is `binary`.
        body: String,
        /// Ordinary message or third-person action.
        #[serde(default)]
        kind: MessageKind,
        /// How to interpret `body`.
        #[serde(default)]
        content_type: ContentType,
    },
    /// A group was joined, or its membership changed.
    GroupChanged {
        /// Stable group identifier.
        group_id: String,
        /// Group name (empty when unknown).
        #[serde(default)]
        group: String,
        /// How many peers are online.
        members: usize,
        /// Whether this session has completed the group handshake.
        joined: bool,
    },
    /// A peer invited us to a group.
    ///
    /// The invitation is never accepted automatically: a front-end has to name
    /// the token with [`Request::JoinGroup`].
    GroupInviteReceived {
        /// Inviting peer's nickname (empty until announced).
        peer: String,
        /// Inviting peer's identifier.
        peer_id: String,
        /// Single-use token accepted by [`Request::JoinGroup`].
        token: String,
    },
    /// The core is stopping; no further replies will be produced.
    Shutdown {
        /// Why the core stopped.
        reason: String,
    },
}

/// Result of a [`Request`], tagged so the two cases cannot be confused.
///
/// Size follows [`Reply`], for the same reason: see the note there on why neither
/// is boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseResult {
    /// The request succeeded.
    Ok {
        /// Reply payload.
        reply: Reply,
    },
    /// The request failed; the session is still alive unless critical.
    Err {
        /// Machine readable failure.
        error: ErrorInfo,
    },
}

impl ResponseResult {
    /// Wrap a successful reply.
    #[must_use]
    pub const fn ok(reply: Reply) -> Self {
        Self::Ok { reply }
    }

    /// Wrap a failure.
    #[must_use]
    pub const fn err(error: ErrorInfo) -> Self {
        Self::Err { error }
    }

    /// Whether this result is a success.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    /// Consume the result, yielding the reply or the error.
    ///
    /// # Errors
    ///
    /// Returns the contained [`ErrorInfo`] when the request failed.
    pub fn into_result(self) -> Result<Reply, ErrorInfo> {
        match self {
            Self::Ok { reply } => Ok(reply),
            Self::Err { error } => Err(error),
        }
    }
}

/// First message a client must send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Greeting; must be the first frame on a connection.
    Hello {
        /// Protocol version the client speaks.
        protocol_version: u32,
        /// Shared secret, required when the server was started with a token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// Free-form client name, used for logging only.
        client: String,
    },
    /// A request correlated by `id`.
    Request {
        /// Client-chosen correlation identifier.
        id: u64,
        /// Operation to perform.
        request: Request,
    },
    /// Polite disconnect; the server stops streaming events for this client.
    Goodbye,
}

/// Messages produced by the core towards a client.
///
/// Size follows [`Reply`] through [`ServerMessage::Response`], for the same
/// reason: see the note on [`Reply`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Successful handshake, sent exactly once per connection.
    Welcome {
        /// Protocol version the server speaks.
        protocol_version: u32,
        /// Session snapshot at handshake time.
        session: Box<SessionInfo>,
    },
    /// Reply to a [`ClientMessage::Request`].
    Response {
        /// Correlation identifier echoed from the request.
        id: u64,
        /// Outcome.
        result: ResponseResult,
    },
    /// Unsolicited notification.
    Event {
        /// The event.
        event: CoreEvent,
    },
    /// The handshake failed; the server closes the connection afterwards.
    Rejected {
        /// Why the connection was refused.
        error: ErrorInfo,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every request/response/event survives a JSON round trip unchanged.
    #[test]
    fn test_protocol_roundtrip_is_lossless() {
        let samples = vec![
            ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                token: None,
                client: "tui/0.4.0".to_string(),
            },
            ClientMessage::Request {
                id: 7,
                request: Request::Ping {
                    echo: Some("hi".to_string()),
                },
            },
            ClientMessage::Request {
                id: 8,
                request: Request::SendMessage {
                    target: Some("Alice".to_string()),
                    text: "hello".to_string(),
                    kind: MessageKind::Text,
                    content_type: ContentType::Text,
                },
            },
            ClientMessage::Goodbye,
        ];

        for message in samples {
            let json = serde_json::to_string(&message).expect("serialize");
            let decoded: ClientMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, message);
        }

        let replies = vec![
            Reply::Pong {
                echo: Some("hi".to_string()),
            },
            Reply::Updated {
                subject: "nickname".to_string(),
                detail: "Alice".to_string(),
            },
            Reply::ShuttingDown { acknowledged: true },
        ];

        for reply in replies {
            let json = serde_json::to_string(&reply).expect("serialize");
            let decoded: Reply = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, reply);
        }
    }

    /// The friend-request vocabulary round-trips, including the reject that lets a
    /// user discard a request instead of only being able to accept it.
    #[test]
    fn test_friend_request_vocabulary_round_trips() {
        // A Tox public key in the form the wire carries it. The length is a wire
        // constant of this crate, so the test does not depend on `tox` (behind
        // the `tox-protocol` feature) or on any layer above.
        let key = "AB".repeat(crate::PUBLIC_KEY_HEX_LEN / 2);

        let requests = vec![
            Request::PeerRequests,
            Request::AcceptPeerRequest {
                public_key: key.clone(),
            },
            Request::RejectPeerRequest {
                public_key: key.clone(),
            },
        ];
        for request in requests {
            let json = serde_json::to_string(&request).expect("serialize");
            let decoded: Request = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, request);
        }

        let replies = vec![
            Reply::PeerRequests {
                requests: vec![PeerRequestView {
                    public_key: key.clone(),
                    message: "hi".to_string(),
                    short_key: "ABAB…ABAB".to_string(),
                }],
            },
            Reply::PeerRequestAccepted {
                public_key: key.clone(),
            },
            Reply::PeerRequestRejected { public_key: key },
        ];
        for reply in replies {
            let json = serde_json::to_string(&reply).expect("serialize");
            let decoded: Reply = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, reply);
        }
    }

    /// Group requests and events round-trip, and an older client's event still
    /// decodes (the new fields are defaulted).
    ///
    /// One test rather than six: the point is that the *whole* group vocabulary
    /// keeps its wire form together, and a test per variant would not notice a
    /// rename that happened to leave each individual assertion valid.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn test_group_vocabulary_round_trips() {
        let requests = vec![
            Request::Groups,
            Request::CreateGroup {
                title: Some("Team".to_string()),
            },
            Request::JoinGroup {
                token: "00ff".to_string(),
            },
            Request::DeclineGroupInvite {
                token: "00ff".to_string(),
            },
            Request::RenameGroup {
                group_id: None,
                name: "Renamed".to_string(),
            },
            Request::InviteToGroup {
                group_id: "ab".repeat(32),
                peer: "Alice".to_string(),
            },
            Request::SendGroupMessage {
                group_id: None,
                text: "hello".to_string(),
                kind: MessageKind::Action,
                content_type: ContentType::Binary,
            },
            Request::LeaveGroup { group_id: None },
            Request::GroupInvites,
        ];
        for request in requests {
            let json = serde_json::to_string(&request).expect("serialize");
            let decoded: Request = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, request);
        }

        // The reply that makes a late-attached front-end able to join: the list
        // carries the token, and an empty list still carries the capability flag.
        for reply in [
            Reply::GroupInvites {
                supported: true,
                invites: vec![GroupInviteView {
                    peer: "Alice".to_string(),
                    peer_id: "D5F0".to_string(),
                    token: "00ff".to_string(),
                }],
            },
            Reply::GroupInvites {
                supported: false,
                invites: Vec::new(),
            },
            Reply::GroupRenamed {
                group_id: "ab".repeat(32),
                group: "Renamed".to_string(),
            },
            Reply::GroupInviteDeclined {
                token: "00ff".to_string(),
            },
        ] {
            let json = serde_json::to_string(&reply).expect("serialize");
            let decoded: Reply = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, reply);
        }

        let events = vec![
            CoreEvent::GroupMessageReceived {
                group_id: "ab".repeat(32),
                group: "Team".to_string(),
                peer: "Alice".to_string(),
                peer_id: "ab".repeat(32) + "#1",
                wire_bytes: 5,
                body: "hello".to_string(),
                kind: MessageKind::Text,
                content_type: ContentType::Text,
            },
            CoreEvent::GroupChanged {
                group_id: "ab".repeat(32),
                group: "Team".to_string(),
                members: 2,
                joined: true,
            },
            CoreEvent::GroupInviteReceived {
                peer: "Alice".to_string(),
                peer_id: "D5F0".to_string(),
                token: "00ff".to_string(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).expect("serialize");
            let decoded: CoreEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, event);
        }

        // A version 4 client never saw `group`, `peer_id`, `kind` or
        // `content_type` on a group message, so those have to default.
        let legacy = r#"{
            "type": "group_message_received",
            "group_id": "ab",
            "peer": "Alice",
            "wire_bytes": 2,
            "body": "hi"
        }"#;
        let decoded: CoreEvent = serde_json::from_str(legacy).expect("a legacy event must decode");
        match decoded {
            CoreEvent::GroupMessageReceived {
                group,
                peer_id,
                kind,
                content_type,
                ..
            } => {
                assert!(group.is_empty());
                assert!(peer_id.is_empty());
                assert_eq!(kind, MessageKind::Text);
                assert_eq!(content_type, ContentType::Text);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Events keep their discriminator so front-ends can switch on the tag.
    #[test]
    fn test_core_event_tag_is_stable() {
        let event = CoreEvent::MessageReceived {
            peer: "Bob".to_string(),
            peer_id: "127.0.0.1:1".to_string(),
            body: "00ff".to_string(),
            wire_bytes: 24,
            kind: MessageKind::Text,
            content_type: ContentType::Binary,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"type\":\"message_received\""));

        let decoded: CoreEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, event);
    }

    /// Unknown request tags are rejected instead of being misinterpreted.
    #[test]
    fn test_unknown_request_tag_is_rejected() {
        let json = r#"{"type":"teleport"}"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }

    /// `ResponseResult` maps onto a plain `Result` without losing the error.
    #[test]
    fn test_response_result_into_result() {
        let ok = ResponseResult::ok(Reply::Pong { echo: None });
        assert!(ok.is_ok());
        assert!(ok.into_result().is_ok());

        let err = ResponseResult::err(ErrorInfo::new(ErrorCode::NotFound, "gone"));
        assert!(!err.is_ok());
        assert_eq!(
            err.into_result().expect_err("must fail").code,
            ErrorCode::NotFound
        );
    }

    /// Internal errors are escalated to critical, domain errors are not.
    #[test]
    fn test_error_severity_mapping() {
        let internal = ErrorInfo::from(MetaTextError::Internal {
            message: "boom".to_string(),
            component: "test".to_string(),
            source: None,
        });
        assert_eq!(internal.code, ErrorCode::Internal);
        assert_eq!(internal.severity, Severity::Critical);

        let network = ErrorInfo::from(MetaTextError::Network {
            message: "down".to_string(),
            operation: "connect".to_string(),
            source: None,
        });
        assert_eq!(network.code, ErrorCode::Network);
        assert_eq!(network.severity, Severity::Recoverable);
        assert!(!network.code.is_critical());
    }
}
