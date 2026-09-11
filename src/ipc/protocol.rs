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

/// Wire protocol version.
///
/// Bumped whenever the shape of [`Request`], [`Reply`] or [`CoreEvent`]
/// changes in a way that is not backwards compatible. A server rejects any
/// client that does not announce exactly this value, which prevents a stale
/// front-end from silently mis-decoding replies.
///
/// History:
/// - `1` — initial request/reply/event vocabulary.
/// - `2` — added [`CoreEvent::MessageUndecodable`]; undecodable payloads are
///   reported instead of being replaced with U+FFFD.
pub const PROTOCOL_VERSION: u32 = 2;

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
/// use meta_text::ipc::protocol::contains_forbidden_control;
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
    /// use meta_text::ipc::protocol::ErrorCode;
    ///
    /// assert!(!ErrorCode::NotFound.is_critical());
    /// assert!(ErrorCode::Internal.is_critical());
    /// ```
    #[must_use]
    pub const fn is_critical(self) -> bool {
        matches!(self, ErrorCode::Internal)
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
    /// use meta_text::ipc::protocol::{ErrorCode, ErrorInfo};
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
        /// Message body.
        text: String,
    },

    /// The most recent persisted messages.
    History {
        /// Maximum number of records; `None` uses the front-end default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },

    /// Persist nickname, contacts and statistics to disk.
    SaveSession,

    /// Stop the core service after replying.
    Shutdown,
}

/// Data returned for a successful [`Request`].
///
/// Replies are pure data; localisation and layout stay in the front-end.
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Local nickname announced to peers.
    pub nickname: String,
    /// Local status message.
    pub status_message: String,
    /// Per-session display identity (never the encryption key).
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
    /// A frame was decrypted but its payload is not valid UTF-8 text.
    ///
    /// Reported explicitly rather than being replaced with U+FFFD, so a
    /// front-end can tell the user that something arrived but could not be
    /// displayed. The frame is still acknowledged at the transport level.
    MessageUndecodable {
        /// Nickname announced by the sender.
        peer: String,
        /// Remote address of the sender.
        peer_id: String,
        /// Ciphertext length in bytes.
        wire_bytes: u64,
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
    /// The core is stopping; no further replies will be produced.
    Shutdown {
        /// Why the core stopped.
        reason: String,
    },
}

/// Result of a [`Request`], tagged so the two cases cannot be confused.
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
    pub fn err(error: ErrorInfo) -> Self {
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

    /// Events keep their discriminator so front-ends can switch on the tag.
    #[test]
    fn test_core_event_tag_is_stable() {
        let event = CoreEvent::MessageReceived {
            peer: "Bob".to_string(),
            peer_id: "127.0.0.1:1".to_string(),
            body: "ping".to_string(),
            wire_bytes: 24,
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
