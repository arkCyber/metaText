/*!
 * database.rs
 *
 * Database operations for metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 */

use crate::error::{MetaTextError, MetaTextResult};
use crate::types::Contact;
use chrono::{DateTime, Utc};
use tracing::{info, warn};

#[cfg(feature = "sqlite")]
use crate::types::UserStatus;
#[cfg(feature = "sqlite")]
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
#[cfg(feature = "sqlite")]
use sqlx::{Row, SqlitePool};
#[cfg(feature = "sqlite")]
use std::time::Duration;

/// Direction of a stored chat message.
///
/// Persisted together with every message so that a restored history can be
/// rendered with the correct arrow (incoming vs. outgoing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDirection {
    /// Message received from a peer.
    Incoming,

    /// Message sent by the local user.
    Outgoing,
}

impl MessageDirection {
    /// Stable storage name used in the database.
    ///
    /// # Returns
    ///
    /// Returns `"in"` for incoming and `"out"` for outgoing messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Incoming => "in",
            Self::Outgoing => "out",
        }
    }

    /// Parse a direction from its stable storage name.
    ///
    /// # Arguments
    ///
    /// * `value` - Name previously produced by [`MessageDirection::as_str`].
    ///
    /// # Returns
    ///
    /// Returns [`MessageDirection::Outgoing`] for `"out"`, and
    /// [`MessageDirection::Incoming`] for every other value.
    #[must_use]
    pub fn from_db(value: &str) -> Self {
        if value == "out" {
            Self::Outgoing
        } else {
            Self::Incoming
        }
    }
}

/// A persisted chat message.
///
/// The record deliberately stores only what is needed to render a local
/// history: the direction, the conversation partner, the plaintext body and
/// the number of bytes that went over the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    /// Database row id (`0` before the record has been inserted).
    pub id: i64,

    /// Whether the message was sent or received.
    pub direction: MessageDirection,

    /// Conversation partner (contact name or identifier).
    pub peer: String,

    /// Message body.
    pub body: String,

    /// Number of bytes that travelled over the wire (ciphertext length).
    pub wire_bytes: u64,

    /// Creation timestamp in UTC.
    pub created_at: DateTime<Utc>,
}

impl StoredMessage {
    /// Create a new unsaved message record stamped with the current time.
    ///
    /// # Arguments
    ///
    /// * `direction` - Whether the message was sent or received.
    /// * `peer` - Conversation partner.
    /// * `body` - Message body.
    /// * `wire_bytes` - Number of bytes sent over the wire.
    ///
    /// # Returns
    ///
    /// Returns a [`StoredMessage`] with an unassigned id (`0`).
    #[must_use]
    pub fn new(
        direction: MessageDirection,
        peer: impl Into<String>,
        body: impl Into<String>,
        wire_bytes: u64,
    ) -> Self {
        Self {
            id: 0,
            direction,
            peer: peer.into(),
            body: body.into(),
            wire_bytes,
            created_at: Utc::now(),
        }
    }
}

/// Database manager for handling data persistence.
///
/// When the crate is built with the `sqlite` feature and the configured
/// `database_type` is `sqlite`, this manager owns a real connection pool,
/// creates the schema on demand and exposes CRUD helpers for contacts and
/// message history. Without the feature the manager degrades to a
/// pass-through that only tracks lifecycle state, so the rest of the
/// application code does not have to special case the disabled state.
#[derive(Debug)]
pub struct DatabaseManager {
    /// Database connection string
    connection_string: String,

    /// Maximum database connections
    max_connections: u32,

    /// Configured database type (sqlite, postgres, mysql)
    database_type: String,

    /// Whether schema migrations should be applied on startup
    enable_migrations: bool,

    /// Whether the database is initialized
    initialized: bool,

    /// Live database connection pool (only present with the `sqlite` feature)
    #[cfg(feature = "sqlite")]
    pool: Option<SqlitePool>,
}

impl DatabaseManager {
    /// Create a new database manager
    ///
    /// # Errors
    ///
    /// Currently this only fails if the logging subsystem rejects a message,
    /// but the `Result` is kept so future backends can validate the
    /// configuration before a connection is opened.
    pub async fn new(config: &crate::config::DatabaseConfig) -> MetaTextResult<Self> {
        let timestamp = chrono::Utc::now();
        info!(
            "💾 [{}] Initializing database manager",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        let manager = Self {
            connection_string: config.connection_string.clone(),
            max_connections: config.max_connections,
            database_type: config.database_type.clone(),
            enable_migrations: config.enable_migrations,
            initialized: false,
            #[cfg(feature = "sqlite")]
            pool: None,
        };

        info!(
            "✅ [{}] Database manager initialized with connection: {}",
            timestamp.format("%Y-%m-%d %H:%M:%S"),
            manager.connection_string
        );

        Ok(manager)
    }

    /// Initialize the database connection
    ///
    /// When built with the `sqlite` feature and configured for the `SQLite`
    /// backend, this opens (and creates if missing) the database file,
    /// applies the schema and keeps a connection pool alive until
    /// [`DatabaseManager::shutdown`] is called.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] when the connection or the schema
    /// migration fails.
    pub async fn start(&mut self) -> MetaTextResult<()> {
        let timestamp = chrono::Utc::now();
        info!(
            "🔌 [{}] Starting database connection",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        if self.initialized {
            warn!(
                "⚠️ [{}] Database connection already started",
                timestamp.format("%Y-%m-%d %H:%M:%S")
            );
            return Ok(());
        }

        #[cfg(feature = "sqlite")]
        {
            if self.database_type.eq_ignore_ascii_case("sqlite") {
                // A fresh `--data-dir` may not exist yet. SQLite creates the
                // database file but never its parent directories, so make sure
                // they exist first (skip in-memory databases).
                if self.connection_string != ":memory:" {
                    let path = std::path::Path::new(&self.connection_string);
                    if let Some(parent) = path.parent() {
                        if !parent.as_os_str().is_empty() {
                            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                                MetaTextError::Database {
                                    message: format!(
                                        "Failed to create database directory {}: {error}",
                                        parent.display()
                                    ),
                                    operation: "connect".to_string(),
                                    source: None,
                                }
                            })?;
                        }
                    }
                }

                let options = SqliteConnectOptions::new()
                    .filename(&self.connection_string)
                    .create_if_missing(true)
                    .busy_timeout(Duration::from_secs(5));

                let pool = SqlitePoolOptions::new()
                    .max_connections(self.max_connections.max(1))
                    .connect_with(options)
                    .await
                    .map_err(|error| MetaTextError::Database {
                        message: format!(
                            "Failed to open SQLite database {}: {error}",
                            self.connection_string
                        ),
                        operation: "connect".to_string(),
                        source: None,
                    })?;

                if self.enable_migrations {
                    Self::run_migrations(&pool).await?;
                }

                self.pool = Some(pool);
                info!(
                    "✅ [{}] SQLite database opened at {}",
                    timestamp.format("%Y-%m-%d %H:%M:%S"),
                    self.connection_string
                );
            } else {
                warn!(
                    "⚠️ [{}] Unsupported database type '{}'; persistence disabled",
                    timestamp.format("%Y-%m-%d %H:%M:%S"),
                    self.database_type
                );
            }
        }

        #[cfg(not(feature = "sqlite"))]
        {
            warn!(
                "⚠️ [{}] Built without the `sqlite` feature (database_type='{}', migrations={}); \
                 persistence is disabled. Rebuild with `--features sqlite` to enable it.",
                timestamp.format("%Y-%m-%d %H:%M:%S"),
                self.database_type,
                self.enable_migrations
            );
        }

        self.initialized = true;

        info!(
            "✅ [{}] Database connection established",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );
        Ok(())
    }

    /// Shutdown the database connection
    ///
    /// # Errors
    ///
    /// This implementation never fails, but keeps the `Result` return type so
    /// that future backends can report errors.
    pub async fn shutdown(&mut self) -> MetaTextResult<()> {
        let timestamp = chrono::Utc::now();
        info!(
            "🔌 [{}] Shutting down database connection",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );

        #[cfg(feature = "sqlite")]
        if let Some(pool) = self.pool.take() {
            // `close()` waits for in-flight queries and closes every
            // connection in the pool.
            pool.close().await;
        }

        self.initialized = false;

        info!(
            "✅ [{}] Database connection closed",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );
        Ok(())
    }

    /// Check if database is initialized
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Check whether durable persistence is actually available.
    ///
    /// # Returns
    ///
    /// Returns `true` only when a live backend connection pool exists, which
    /// requires the `sqlite` feature and a successful
    /// [`DatabaseManager::start`] call.
    #[must_use]
    pub const fn is_persistent(&self) -> bool {
        #[cfg(feature = "sqlite")]
        {
            self.pool.is_some()
        }
        #[cfg(not(feature = "sqlite"))]
        {
            false
        }
    }

    /// Get the specified connection string
    ///
    /// # Returns
    ///
    /// Returns the database connection string this manager was created with.
    #[must_use]
    pub fn connection_string(&self) -> &str {
        &self.connection_string
    }

    /// Get the maximum number of database connections
    ///
    /// # Returns
    ///
    /// Returns the configured connection pool size.
    #[must_use]
    pub const fn max_connections(&self) -> u32 {
        self.max_connections
    }
}

/// Real database persistence, only compiled when the `sqlite` feature is on.
#[cfg(feature = "sqlite")]
impl DatabaseManager {
    /// Create the schema used by metaText.
    ///
    /// The statements are idempotent (`IF NOT EXISTS`) so they are safe to run
    /// on every startup.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] if a statement fails.
    async fn run_migrations(pool: &SqlitePool) -> MetaTextResult<()> {
        const STATEMENTS: &[&str] = &[
            "CREATE TABLE IF NOT EXISTS contacts (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                public_key BLOB NOT NULL,
                status TEXT NOT NULL,
                status_message TEXT,
                added_at TEXT NOT NULL,
                last_seen TEXT,
                avatar_hash TEXT,
                note TEXT,
                is_blocked INTEGER NOT NULL DEFAULT 0
            )",
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                direction TEXT NOT NULL,
                peer TEXT NOT NULL,
                body TEXT NOT NULL,
                wire_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at)",
            "CREATE INDEX IF NOT EXISTS idx_messages_peer ON messages(peer)",
        ];

        for statement in STATEMENTS {
            sqlx::query(statement)
                .execute(pool)
                .await
                .map_err(|error| MetaTextError::Database {
                    message: format!("Schema migration failed: {error}"),
                    operation: "migrate".to_string(),
                    source: None,
                })?;
        }

        Ok(())
    }

    /// Parse a UUID column, mapping failures to a database error.
    fn parse_uuid(value: &str) -> MetaTextResult<uuid::Uuid> {
        uuid::Uuid::parse_str(value).map_err(|error| MetaTextError::Database {
            message: format!("Invalid contact id '{value}': {error}"),
            operation: "load_contacts".to_string(),
            source: None,
        })
    }

    /// Parse an RFC 3339 timestamp column into UTC.
    fn parse_datetime(value: &str) -> MetaTextResult<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(value)
            .map(|parsed| parsed.with_timezone(&Utc))
            .map_err(|error| MetaTextError::Database {
                message: format!("Invalid timestamp '{value}': {error}"),
                operation: "timestamp".to_string(),
                source: None,
            })
    }

    /// Borrow the live pool or fail with a descriptive error.
    fn pool(&self) -> MetaTextResult<&SqlitePool> {
        self.pool.as_ref().ok_or_else(|| MetaTextError::Database {
            message: "SQLite pool is not available; call start() first".to_string(),
            operation: "pool".to_string(),
            source: None,
        })
    }

    /// Insert or update a contact.
    ///
    /// # Arguments
    ///
    /// * `contact` - Contact to persist; its `id` is the primary key.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] if the write fails.
    pub async fn save_contact(&self, contact: &Contact) -> MetaTextResult<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO contacts
                (id, name, public_key, status, status_message, added_at, last_seen, avatar_hash, note, is_blocked)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(contact.id.to_string())
        .bind(&contact.name)
        .bind(contact.public_key.clone())
        .bind(contact.status.as_name())
        .bind(contact.status_message.clone())
        .bind(contact.added_at.to_rfc3339())
        .bind(contact.last_seen.map(|value| value.to_rfc3339()))
        .bind(contact.avatar_hash.clone())
        .bind(contact.note.clone())
        .bind(contact.is_blocked)
        .execute(self.pool()?)
        .await
        .map_err(|error| MetaTextError::Database {
            message: format!("Failed to save contact {}: {error}", contact.name),
            operation: "save_contact".to_string(),
            source: None,
        })?;

        Ok(())
    }

    /// Load every stored contact ordered by the time they were added.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] if the query or row decoding fails.
    pub async fn load_contacts(&self) -> MetaTextResult<Vec<Contact>> {
        let rows = sqlx::query(
            "SELECT id, name, public_key, status, status_message, added_at, last_seen, avatar_hash, note, is_blocked
             FROM contacts ORDER BY added_at ASC",
        )
        .fetch_all(self.pool()?)
        .await
        .map_err(|error| MetaTextError::Database {
            message: format!("Failed to load contacts: {error}"),
            operation: "load_contacts".to_string(),
            source: None,
        })?;

        let mut contacts = Vec::with_capacity(rows.len());
        for row in rows {
            let status_name: String =
                row.try_get("status")
                    .map_err(|error| MetaTextError::Database {
                        message: format!("Missing contact status column: {error}"),
                        operation: "load_contacts".to_string(),
                        source: None,
                    })?;
            let last_seen: Option<String> = row.try_get("last_seen").ok().flatten();
            let added_at: String =
                row.try_get("added_at")
                    .map_err(|error| MetaTextError::Database {
                        message: format!("Missing contact added_at column: {error}"),
                        operation: "load_contacts".to_string(),
                        source: None,
                    })?;

            contacts.push(Contact {
                id: Self::parse_uuid(&row.try_get::<String, _>("id").map_err(|error| {
                    MetaTextError::Database {
                        message: format!("Missing contact id column: {error}"),
                        operation: "load_contacts".to_string(),
                        source: None,
                    }
                })?)?,
                name: row.try_get("name").unwrap_or_default(),
                public_key: row.try_get("public_key").unwrap_or_default(),
                status: UserStatus::from_name(&status_name),
                status_message: row.try_get("status_message").ok().flatten(),
                added_at: Self::parse_datetime(&added_at)?,
                last_seen: last_seen.as_deref().map(Self::parse_datetime).transpose()?,
                avatar_hash: row.try_get("avatar_hash").ok().flatten(),
                note: row.try_get("note").ok().flatten(),
                is_blocked: row.try_get("is_blocked").unwrap_or_default(),
            });
        }

        Ok(contacts)
    }

    /// Persist a chat message.
    ///
    /// # Arguments
    ///
    /// * `message` - Message record to store.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] if the write fails.
    pub async fn save_message(&self, message: &StoredMessage) -> MetaTextResult<()> {
        sqlx::query(
            "INSERT INTO messages (direction, peer, body, wire_bytes, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(message.direction.as_str())
        .bind(&message.peer)
        .bind(&message.body)
        .bind(i64::try_from(message.wire_bytes).unwrap_or(i64::MAX))
        .bind(message.created_at.to_rfc3339())
        .execute(self.pool()?)
        .await
        .map_err(|error| MetaTextError::Database {
            message: format!("Failed to save message for {}: {error}", message.peer),
            operation: "save_message".to_string(),
            source: None,
        })?;

        Ok(())
    }

    /// Load the most recent messages in chronological order.
    ///
    /// # Arguments
    ///
    /// * `limit` - Maximum number of messages to return.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] if the query fails.
    pub async fn recent_messages(&self, limit: usize) -> MetaTextResult<Vec<StoredMessage>> {
        // Clamp to the representable positive i64 range so an oversized limit
        // cannot wrap into a negative SQLite `LIMIT` (which means "unlimited").
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = sqlx::query(
            "SELECT id, direction, peer, body, wire_bytes, created_at
             FROM messages ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.pool()?)
        .await
        .map_err(|error| MetaTextError::Database {
            message: format!("Failed to load message history: {error}"),
            operation: "recent_messages".to_string(),
            source: None,
        })?;

        let mut messages = Vec::with_capacity(rows.len());
        for row in rows {
            let direction: String = row.try_get("direction").unwrap_or_default();
            let created_at: String = row.try_get("created_at").unwrap_or_default();
            messages.push(StoredMessage {
                id: row.try_get("id").unwrap_or_default(),
                direction: MessageDirection::from_db(&direction),
                peer: row.try_get("peer").unwrap_or_default(),
                body: row.try_get("body").unwrap_or_default(),
                wire_bytes: u64::try_from(
                    row.try_get::<i64, _>("wire_bytes")
                        .unwrap_or_default()
                        .max(0),
                )
                .unwrap_or(0),
                created_at: Self::parse_datetime(&created_at)?,
            });
        }

        // The query returns newest first for the LIMIT to be meaningful; the
        // caller wants to render them oldest first.
        messages.reverse();
        Ok(messages)
    }

    /// Count the number of stored messages.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] if the query fails.
    pub async fn message_count(&self) -> MetaTextResult<u64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(self.pool()?)
            .await
            .map_err(|error| MetaTextError::Database {
                message: format!("Failed to count messages: {error}"),
                operation: "message_count".to_string(),
                source: None,
            })?;

        Ok(u64::try_from(count).unwrap_or(0))
    }
}

/// Pass-through persistence used when the `sqlite` feature is disabled.
///
/// Every method keeps the same signature as the real backend so callers do not
/// need `cfg` attributes, but since there is no connection pool they report a
/// descriptive error. Callers should gate on [`DatabaseManager::is_persistent`]
/// to avoid surfacing those errors to the user.
#[cfg(not(feature = "sqlite"))]
#[allow(clippy::unused_async)]
impl DatabaseManager {
    /// Build the error returned by every disabled operation.
    fn persistence_disabled(operation: &str) -> MetaTextError {
        MetaTextError::Database {
            message: "Persistence requires a build with the `sqlite` feature".to_string(),
            operation: operation.to_string(),
            source: None,
        }
    }

    /// Insert or update a contact (disabled without the `sqlite` feature).
    ///
    /// # Errors
    ///
    /// Always returns [`MetaTextError::Database`] in this configuration.
    pub async fn save_contact(&self, _contact: &Contact) -> MetaTextResult<()> {
        Err(Self::persistence_disabled("save_contact"))
    }

    /// Load every stored contact (disabled without the `sqlite` feature).
    ///
    /// # Errors
    ///
    /// Always returns [`MetaTextError::Database`] in this configuration.
    pub async fn load_contacts(&self) -> MetaTextResult<Vec<Contact>> {
        Err(Self::persistence_disabled("load_contacts"))
    }

    /// Persist a chat message (disabled without the `sqlite` feature).
    ///
    /// # Errors
    ///
    /// Always returns [`MetaTextError::Database`] in this configuration.
    pub async fn save_message(&self, _message: &StoredMessage) -> MetaTextResult<()> {
        Err(Self::persistence_disabled("save_message"))
    }

    /// Load the most recent messages (disabled without the `sqlite` feature).
    ///
    /// # Errors
    ///
    /// Always returns [`MetaTextError::Database`] in this configuration.
    pub async fn recent_messages(&self, _limit: usize) -> MetaTextResult<Vec<StoredMessage>> {
        Err(Self::persistence_disabled("recent_messages"))
    }

    /// Count the number of stored messages (disabled without `sqlite`).
    ///
    /// # Errors
    ///
    /// Always returns [`MetaTextError::Database`] in this configuration.
    pub async fn message_count(&self) -> MetaTextResult<u64> {
        Err(Self::persistence_disabled("message_count"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;

    #[tokio::test]
    async fn test_database_manager_creation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let config = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string: db_path.clone(),
            max_connections: 5,
            enable_migrations: true,
        };

        let manager = DatabaseManager::new(&config).await.unwrap();
        assert_eq!(manager.connection_string, db_path);
        assert_eq!(manager.max_connections, 5);
        assert!(!manager.is_initialized());
    }

    #[tokio::test]
    async fn test_database_startup_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let config = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string: dir.path().join("test.db").to_string_lossy().to_string(),
            max_connections: 5,
            enable_migrations: true,
        };

        let mut manager = DatabaseManager::new(&config).await.unwrap();

        // Start database
        manager.start().await.unwrap();
        assert!(manager.is_initialized());

        // Shutdown database
        manager.shutdown().await.unwrap();
        assert!(!manager.is_initialized());
    }

    /// The direction enum round-trips through its storage name
    #[test]
    fn test_message_direction_roundtrip() {
        assert_eq!(MessageDirection::Outgoing.as_str(), "out");
        assert_eq!(MessageDirection::Incoming.as_str(), "in");
        assert_eq!(MessageDirection::from_db("out"), MessageDirection::Outgoing);
        assert_eq!(MessageDirection::from_db("in"), MessageDirection::Incoming);
        // Unknown values fall back to incoming rather than panicking.
        assert_eq!(
            MessageDirection::from_db("nonsense"),
            MessageDirection::Incoming
        );
    }

    /// A freshly built message record is unstamped and time-stamped
    #[test]
    fn test_stored_message_new() {
        let message = StoredMessage::new(MessageDirection::Outgoing, "Alice", "hello", 42);
        assert_eq!(message.id, 0);
        assert_eq!(message.direction, MessageDirection::Outgoing);
        assert_eq!(message.peer, "Alice");
        assert_eq!(message.body, "hello");
        assert_eq!(message.wire_bytes, 42);
        assert!(message.created_at <= Utc::now());
    }

    /// Without the `sqlite` feature persistence is reported as unavailable
    #[cfg(not(feature = "sqlite"))]
    #[tokio::test]
    async fn test_persistence_disabled_without_feature() {
        let config = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string: "disabled.db".to_string(),
            max_connections: 1,
            enable_migrations: true,
        };

        let mut manager = DatabaseManager::new(&config).await.unwrap();
        manager.start().await.unwrap();
        assert!(manager.is_initialized());
        assert!(!manager.is_persistent());

        assert!(manager.load_contacts().await.is_err());
        assert!(manager.message_count().await.is_err());
    }

    /// With the `sqlite` feature contacts and messages survive a round-trip
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let config = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string: dir
                .path()
                .join("meta-text.db")
                .to_string_lossy()
                .to_string(),
            max_connections: 2,
            enable_migrations: true,
        };

        let mut manager = DatabaseManager::new(&config).await.unwrap();
        manager.start().await.unwrap();
        assert!(manager.is_persistent());

        // Contacts round-trip with every field preserved.
        let mut contact = Contact::new("Alice".to_string(), vec![1, 2, 3, 4]);
        contact.note = Some("met in the metaverse".to_string());
        contact.status = UserStatus::Online;
        contact.update_last_seen();
        manager.save_contact(&contact).await.unwrap();

        let contacts = manager.load_contacts().await.unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].id, contact.id);
        assert_eq!(contacts[0].name, "Alice");
        assert_eq!(contacts[0].public_key, vec![1, 2, 3, 4]);
        assert_eq!(contacts[0].status, UserStatus::Online);
        assert_eq!(contacts[0].note.as_deref(), Some("met in the metaverse"));

        // Messages round-trip in chronological order.
        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Outgoing,
                "Alice",
                "first",
                10,
            ))
            .await
            .unwrap();
        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Incoming,
                "Alice",
                "second",
                20,
            ))
            .await
            .unwrap();

        assert_eq!(manager.message_count().await.unwrap(), 2);

        let history = manager.recent_messages(10).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].body, "first");
        assert_eq!(history[0].direction, MessageDirection::Outgoing);
        assert_eq!(history[1].body, "second");
        assert_eq!(history[1].direction, MessageDirection::Incoming);

        manager.shutdown().await.unwrap();
        assert!(!manager.is_persistent());
    }

    /// A fresh database path gets its missing parent directories created
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("meta-text.db");

        let config = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string: nested.to_string_lossy().to_string(),
            max_connections: 1,
            enable_migrations: true,
        };

        let mut manager = DatabaseManager::new(&config).await.unwrap();
        manager.start().await.unwrap();

        assert!(manager.is_persistent());
        assert!(nested.exists(), "database file should have been created");

        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Outgoing,
                "Alice",
                "hi",
                2,
            ))
            .await
            .unwrap();
        assert_eq!(manager.message_count().await.unwrap(), 1);

        manager.shutdown().await.unwrap();
    }
}
