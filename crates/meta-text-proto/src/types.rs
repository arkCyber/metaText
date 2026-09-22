/*!
 * types.rs
 *
 * Core type definitions for metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Application event system
 * - Message type definitions
 * - User and contact management
 * - Network protocol types
 */

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// What kind of chat payload a message carries.
///
/// The distinction is a domain concept, not a transport one: Tox has a native
/// `TOX_MESSAGE_TYPE` for it and the TCP transport encodes it in the frame kind,
/// but the actor and the front-ends only care that an action is a body *about*
/// the sender rather than a body *from* them (`/me waves` renders as
/// `* Alice waves`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// An ordinary chat message.
    #[default]
    Text,

    /// A third-person action describing the sender.
    Action,
}

impl MessageKind {
    /// Stable lowercase name, used on the wire and in logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Action => "action",
        }
    }

    /// Parse the storage name written by [`Self::as_str`].
    ///
    /// Anything unrecognised (including a row written before the column existed)
    /// decodes as [`Self::Text`], so an old or damaged row is still displayable
    /// instead of failing the whole history query.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::types::MessageKind;
    ///
    /// assert_eq!(MessageKind::from_db("action"), MessageKind::Action);
    /// assert_eq!(MessageKind::from_db("text"), MessageKind::Text);
    /// assert_eq!(MessageKind::from_db(""), MessageKind::Text);
    /// assert_eq!(MessageKind::from_db("whatever"), MessageKind::Text);
    /// ```
    #[must_use]
    pub const fn from_db(value: &str) -> Self {
        if value.eq_ignore_ascii_case("action") {
            Self::Action
        } else {
            Self::Text
        }
    }
}

impl std::fmt::Display for MessageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the bytes of a message body must be interpreted.
///
/// `kind` says how a *human* reads a message; `content_type` says what the bytes
/// *are*. The two are independent except for one rule: a binary blob is never an
/// action, because `* Alice <opaque bytes>` is meaningless.
///
/// Over the JSON interface a binary body travels as lowercase hexadecimal, so the
/// protocol stays text-only while the transports still carry real bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    /// A UTF-8 text body (the default).
    #[default]
    Text,

    /// An opaque byte body, carried as lowercase hexadecimal on the JSON wire.
    Binary,
}

impl ContentType {
    /// Stable lowercase name, used on the wire and in storage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Binary => "binary",
        }
    }

    /// Whether the body is opaque bytes rather than UTF-8 text.
    #[must_use]
    pub const fn is_binary(self) -> bool {
        matches!(self, Self::Binary)
    }

    /// Parse the storage name written by [`Self::as_str`].
    ///
    /// Anything unrecognised (including a row written before the column existed)
    /// decodes as [`Self::Text`], so an old or damaged row stays displayable.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::types::ContentType;
    ///
    /// assert_eq!(ContentType::from_db("binary"), ContentType::Binary);
    /// assert_eq!(ContentType::from_db("text"), ContentType::Text);
    /// assert_eq!(ContentType::from_db(""), ContentType::Text);
    /// ```
    #[must_use]
    pub const fn from_db(value: &str) -> Self {
        if value.eq_ignore_ascii_case("binary") {
            Self::Binary
        } else {
            Self::Text
        }
    }
}

impl std::fmt::Display for ContentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Application-wide event types for inter-component communication
///
/// These events are used to coordinate between different subsystems
/// of the application, enabling loose coupling and maintainable
/// architecture.
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Request application shutdown
    Shutdown,

    /// Network-related events
    NetworkEvent(NetworkEvent),

    /// User input from CLI or TUI interface
    UserInput(String),

    /// New message received from network
    MessageReceived {
        /// Nickname announced by the sender (empty until the handshake is seen)
        peer: String,

        /// Remote address of the sender
        peer_id: String,

        /// Decrypted message payload
        payload: Vec<u8>,

        /// Whether this is an ordinary message or a third-person action
        kind: MessageKind,

        /// How the payload bytes must be interpreted
        content_type: ContentType,
    },

    /// A peer acknowledged one of our outgoing messages
    MessageDelivered {
        /// Nickname of the peer that acknowledged
        peer: String,

        /// Remote address of the peer that acknowledged
        peer_id: String,

        /// Identifier returned when the message was sent
        message_id: u64,
    },

    /// A peer asked to become a friend.
    ///
    /// Only the Tox transport produces this: it carries the requester's public
    /// key and the message they attached, so the core can surface it to a
    /// front-end for an explicit accept/reject decision. The request is never
    /// accepted automatically.
    PeerRequestReceived {
        /// Requesting peer's public identifier (64 hexadecimal characters).
        peer_id: String,

        /// Message attached to the request (may be empty).
        message: String,
    },

    /// Friend status change notification
    FriendStatusChanged {
        /// Friend's unique identifier
        friend_id: Uuid,
        /// New connection status
        is_online: bool,
    },

    /// A group's membership changed, or a group was joined.
    ///
    /// Produced by the transport for every conference event that changes what the
    /// group *is*: the handshake completing, a peer joining or leaving.
    GroupChanged {
        /// Stable group identifier.
        group_id: String,

        /// Group name (empty when unknown).
        name: String,

        /// How many peers are online in the group.
        members: usize,

        /// Whether this instance has completed the group handshake.
        joined: bool,
    },

    /// A message was received in a group.
    ///
    /// Like [`AppEvent::MessageReceived`], the payload is already decrypted when
    /// the transport provides encryption, and `content_type` says how to read it.
    GroupMessageReceived {
        /// Stable group identifier.
        group_id: String,

        /// Group name (empty when unknown).
        name: String,

        /// Sender's announced name inside the group.
        peer: String,

        /// Sender's address inside the group (`<group id>#<peer number>`).
        peer_id: String,

        /// Message payload.
        payload: Vec<u8>,

        /// Whether this is an ordinary message or a third-person action.
        kind: MessageKind,

        /// How the payload bytes must be interpreted.
        content_type: ContentType,
    },

    /// A peer invited us to a group.
    ///
    /// The invitation is surfaced, never accepted automatically, and it can only
    /// be accepted once: the transport keeps the capability until it is used.
    GroupInviteReceived {
        /// Inviting peer's nickname (empty until announced).
        peer: String,

        /// Inviting peer's address.
        peer_id: String,

        /// Opaque single-use token to pass to `Request::JoinGroup`.
        token: String,
    },
}

/// A group chat (Tox conference) as the core sees it.
///
/// `id` is the transport's stable identifier (a 64 character conference id on
/// Tox), not a locally generated value: it is what survives a restart and what a
/// peer can be invited to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// Stable group identifier.
    pub id: String,

    /// Group name (the conference title; may be empty right after joining).
    pub name: String,

    /// How many peers are online in the group.
    pub members: usize,

    /// Whether this instance has completed the group handshake.
    pub joined: bool,
}

/// Network-specific event types
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    /// New peer connection established
    PeerConnected {
        /// Peer's network identifier
        peer_id: String,
        /// Connection metadata
        metadata: HashMap<String, String>,
    },

    /// Peer disconnection event
    PeerDisconnected {
        /// Peer's network identifier
        peer_id: String,
        /// Reason for disconnection
        reason: String,
    },

    /// Bootstrap node connection status
    BootstrapStatus {
        /// Bootstrap node address
        node_address: String,
        /// Connection successful
        connected: bool,
    },
}

/// User roles within a group chat
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupRole {
    /// Group owner with full permissions
    Owner,

    /// Administrator with management permissions
    Admin,

    /// Moderator with limited management permissions
    Moderator,

    /// Regular group member
    Member,
}

/// Application state container
///
/// Maintains the current state of the application including
/// active conversations, user preferences, and runtime statistics.
#[derive(Debug, Default)]
pub struct AppState {
    /// Currently active conversation
    pub active_conversation: Option<Uuid>,

    /// User's online status
    pub user_status: UserStatus,

    /// Application runtime statistics
    pub statistics: AppStatistics,

    /// Feature flags and preferences
    pub preferences: UserPreferences,
}

impl AppState {
    /// Create a new application state with default values
    ///
    /// # Returns
    ///
    /// Returns a new `AppState` instance with sensible defaults.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::types::{AppState, UserStatus};
    ///
    /// let state = AppState::new();
    /// assert_eq!(state.user_status, UserStatus::Online);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            active_conversation: None,
            user_status: UserStatus::Online,
            statistics: AppStatistics::default(),
            preferences: UserPreferences::default(),
        }
    }
}

/// User's current status in the application
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UserStatus {
    /// User is online and available
    #[default]
    Online,

    /// User is away from keyboard
    Away,

    /// User is busy, do not disturb
    Busy,

    /// User appears offline to others
    Invisible,

    /// User is offline
    Offline,
}

impl UserStatus {
    /// Get the stable storage/display name for this status
    ///
    /// # Returns
    ///
    /// Returns a lowercase, stable string representation that is safe to store
    /// in a database and to send over the wire.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::types::UserStatus;
    ///
    /// assert_eq!(UserStatus::Online.as_name(), "online");
    /// ```
    #[must_use]
    pub const fn as_name(&self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Away => "away",
            Self::Busy => "busy",
            Self::Invisible => "invisible",
            Self::Offline => "offline",
        }
    }

    /// Parse a status from its stable storage/display name
    ///
    /// # Arguments
    ///
    /// * `name` - Name previously produced by [`UserStatus::as_name`]
    ///
    /// # Returns
    ///
    /// Returns the matching status, defaulting to [`UserStatus::Offline`] for
    /// unknown values.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::types::UserStatus;
    ///
    /// assert_eq!(UserStatus::from_name("busy"), UserStatus::Busy);
    /// assert_eq!(UserStatus::from_name("unknown"), UserStatus::Offline);
    /// ```
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "online" => Self::Online,
            "away" => Self::Away,
            "busy" => Self::Busy,
            "invisible" => Self::Invisible,
            _ => Self::Offline,
        }
    }
}

/// Runtime statistics for the application
///
/// Tracks various metrics about application usage and performance
/// for monitoring and debugging purposes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppStatistics {
    /// Total messages sent since startup
    pub messages_sent: u64,

    /// Total messages received since startup
    pub messages_received: u64,

    /// Number of active connections
    pub active_connections: u32,

    /// Application start time
    pub start_time: Option<DateTime<Utc>>,

    /// Total bytes sent
    pub bytes_sent: u64,

    /// Total bytes received
    pub bytes_received: u64,
}

/// User preferences and configuration
///
/// Stores user-specific settings and preferences that persist
/// across application sessions.
///
/// Preferences are a serialized record, so each flag is an independent boolean a
/// user toggles and an older file keeps loading (every field is defaulted). The
/// lint's suggestion — one enum per pair of flags — would make "absent" and
/// "false" indistinguishable on the wire, which is the opposite of what a
/// configuration file needs.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize, Deserialize)]
pub struct UserPreferences {
    /// Enable desktop notifications
    pub notifications_enabled: bool,

    /// Auto-save conversations
    pub auto_save_conversations: bool,

    /// Default message encryption
    pub encryption_enabled: bool,

    /// Theme selection
    pub theme: ThemeSelection,

    /// Maximum message history to keep
    pub max_history_size: usize,

    /// Auto-connect on startup
    pub auto_connect: bool,
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            notifications_enabled: true,
            auto_save_conversations: true,
            encryption_enabled: true,
            theme: ThemeSelection::Dark,
            max_history_size: 10000,
            auto_connect: true,
        }
    }
}

/// Available theme options for the user interface
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeSelection {
    /// Dark theme for low-light environments
    Dark,

    /// Light theme for bright environments
    Light,

    /// High contrast theme for accessibility
    HighContrast,

    /// Custom user-defined theme
    Custom(String),
}

/// Shutdown signal types for graceful application termination
#[derive(Debug, Clone)]
pub enum ShutdownSignal {
    /// User-initiated shutdown
    UserRequested,

    /// System signal (SIGTERM, SIGINT)
    SystemSignal(String),

    /// Critical error requiring shutdown
    CriticalError(String),

    /// Resource exhaustion
    ResourceExhaustion,
}

/// Message priority levels for queue management
///
/// Used to prioritize message delivery and processing
/// in high-traffic scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum MessagePriority {
    /// Low priority, best-effort delivery
    Low = 0,

    /// Normal priority (default)
    #[default]
    Normal = 1,

    /// High priority, expedited processing
    High = 2,

    /// Critical priority, immediate processing
    Critical = 3,
}

/// Contact information structure
///
/// Represents a friend or contact in the user's contact list
/// with associated metadata and status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    /// Unique identifier for this contact
    pub id: Uuid,

    /// Display name
    pub name: String,

    /// Public key for encryption
    pub public_key: Vec<u8>,

    /// Current online status
    pub status: UserStatus,

    /// Custom status message
    pub status_message: Option<String>,

    /// Contact added timestamp
    pub added_at: DateTime<Utc>,

    /// Last seen timestamp
    pub last_seen: Option<DateTime<Utc>>,

    /// Contact avatar hash
    pub avatar_hash: Option<String>,

    /// Custom note about this contact
    pub note: Option<String>,

    /// Contact is blocked
    pub is_blocked: bool,
}

impl Contact {
    /// Create a new contact with minimal required information
    ///
    /// # Arguments
    ///
    /// * `name` - Display name for the contact
    /// * `public_key` - Cryptographic public key for secure communication
    ///
    /// # Returns
    ///
    /// Returns a new `Contact` instance with default values for optional fields.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_proto::types::Contact;
    ///
    /// let public_key = vec![1, 2, 3, 4]; // Example key
    /// let contact = Contact::new("Alice".to_string(), public_key);
    /// assert_eq!(contact.name, "Alice");
    /// assert!(!contact.is_blocked);
    /// ```
    #[must_use]
    pub fn new(name: String, public_key: Vec<u8>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            public_key,
            status: UserStatus::Offline,
            status_message: None,
            added_at: Utc::now(),
            last_seen: None,
            avatar_hash: None,
            note: None,
            is_blocked: false,
        }
    }

    /// Check if contact is currently online
    ///
    /// # Returns
    ///
    /// Returns `true` if the contact's status indicates they are online.
    #[must_use]
    pub const fn is_online(&self) -> bool {
        matches!(
            self.status,
            UserStatus::Online | UserStatus::Away | UserStatus::Busy
        )
    }

    /// Update the contact's last seen timestamp to current time
    pub fn update_last_seen(&mut self) {
        self.last_seen = Some(Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test contact creation with basic functionality
    #[test]
    fn test_contact_creation() {
        let name = "Test User".to_string();
        let public_key = vec![1, 2, 3, 4, 5];

        let contact = Contact::new(name.clone(), public_key.clone());

        assert_eq!(contact.name, name);
        assert_eq!(contact.public_key, public_key);
        assert!(!contact.is_blocked);
        assert_eq!(contact.status, UserStatus::Offline);
    }

    /// Test contact online status detection
    #[test]
    fn test_contact_online_status() {
        let mut contact = Contact::new("Test".to_string(), vec![1, 2, 3]);

        // Initially offline
        assert!(!contact.is_online());

        // Set to online
        contact.status = UserStatus::Online;
        assert!(contact.is_online());

        // Set to away
        contact.status = UserStatus::Away;
        assert!(contact.is_online());

        // Set to busy
        contact.status = UserStatus::Busy;
        assert!(contact.is_online());

        // Set to invisible
        contact.status = UserStatus::Invisible;
        assert!(!contact.is_online());

        // Set to offline
        contact.status = UserStatus::Offline;
        assert!(!contact.is_online());
    }

    /// Test contact last seen timestamp update
    #[test]
    fn test_contact_last_seen_update() {
        let mut contact = Contact::new("Test".to_string(), vec![1, 2, 3]);

        // Initially no last seen
        assert!(contact.last_seen.is_none());

        // Update last seen
        contact.update_last_seen();
        assert!(contact.last_seen.is_some());

        // Verify it's recent
        let last_seen = contact.last_seen.unwrap();
        let now = Utc::now();
        let diff = now.signed_duration_since(last_seen);
        assert!(diff.num_seconds() < 1); // Should be within 1 second
    }

    /// Test application state initialization
    #[test]
    fn test_app_state_initialization() {
        let state = AppState::new();

        assert!(state.active_conversation.is_none());
        assert_eq!(state.user_status, UserStatus::Online);
        assert_eq!(state.statistics.messages_sent, 0);
        assert_eq!(state.statistics.messages_received, 0);
    }

    /// Test user preferences default values
    #[test]
    fn test_user_preferences_defaults() {
        let prefs = UserPreferences::default();

        assert!(prefs.notifications_enabled);
        assert!(prefs.auto_save_conversations);
        assert!(prefs.encryption_enabled);
        assert_eq!(prefs.theme, ThemeSelection::Dark);
        assert_eq!(prefs.max_history_size, 10000);
        assert!(prefs.auto_connect);
    }

    /// Test message priority ordering
    #[test]
    fn test_message_priority_ordering() {
        assert!(MessagePriority::Low < MessagePriority::Normal);
        assert!(MessagePriority::Normal < MessagePriority::High);
        assert!(MessagePriority::High < MessagePriority::Critical);

        assert_eq!(MessagePriority::default(), MessagePriority::Normal);
    }
}
