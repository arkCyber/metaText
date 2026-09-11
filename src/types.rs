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

    /// Friend status change notification
    FriendStatusChanged {
        /// Friend's unique identifier
        friend_id: Uuid,
        /// New connection status
        is_online: bool,
    },

    /// Group event notification
    GroupEvent {
        /// Group unique identifier
        group_id: Uuid,
        /// Type of group event
        event_type: GroupEventType,
    },
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

/// Group event types for chat room management
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GroupEventType {
    /// New member joined the group
    MemberJoined(String),

    /// Member left the group
    MemberLeft(String),

    /// Group title changed
    TitleChanged(String),

    /// Member role updated
    RoleUpdated {
        /// Member identifier
        member_id: String,
        /// New role assignment
        new_role: GroupRole,
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
    /// use meta_text::types::{AppState, UserStatus};
    ///
    /// let state = AppState::new();
    /// assert_eq!(state.user_status, UserStatus::Online);
    /// ```
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UserStatus {
    /// User is online and available
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

impl Default for UserStatus {
    fn default() -> Self {
        Self::Online
    }
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
    /// use meta_text::types::UserStatus;
    ///
    /// assert_eq!(UserStatus::Online.as_name(), "online");
    /// ```
    pub fn as_name(&self) -> &'static str {
        match self {
            UserStatus::Online => "online",
            UserStatus::Away => "away",
            UserStatus::Busy => "busy",
            UserStatus::Invisible => "invisible",
            UserStatus::Offline => "offline",
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
    /// use meta_text::types::UserStatus;
    ///
    /// assert_eq!(UserStatus::from_name("busy"), UserStatus::Busy);
    /// assert_eq!(UserStatus::from_name("unknown"), UserStatus::Offline);
    /// ```
    pub fn from_name(name: &str) -> Self {
        match name {
            "online" => UserStatus::Online,
            "away" => UserStatus::Away,
            "busy" => UserStatus::Busy,
            "invisible" => UserStatus::Invisible,
            _ => UserStatus::Offline,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MessagePriority {
    /// Low priority, best-effort delivery
    Low = 0,

    /// Normal priority (default)
    Normal = 1,

    /// High priority, expedited processing
    High = 2,

    /// Critical priority, immediate processing
    Critical = 3,
}

impl Default for MessagePriority {
    fn default() -> Self {
        Self::Normal
    }
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
    /// use meta_text::types::Contact;
    ///
    /// let public_key = vec![1, 2, 3, 4]; // Example key
    /// let contact = Contact::new("Alice".to_string(), public_key);
    /// assert_eq!(contact.name, "Alice");
    /// assert!(!contact.is_blocked);
    /// ```
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
    pub fn is_online(&self) -> bool {
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
