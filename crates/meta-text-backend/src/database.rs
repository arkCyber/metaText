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

use crate::types::ContentType;
use crate::types::MessageKind;
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

/// Whether a persisted message belongs to a direct conversation or a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConversationKind {
    /// A one-to-one conversation.
    #[default]
    Direct,

    /// A group chat; `StoredMessage::group_id` identifies which one.
    Group,
}

impl ConversationKind {
    /// Stable storage name used in the database.
    ///
    /// # Returns
    ///
    /// Returns `"direct"` or `"group"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Group => "group",
        }
    }

    /// Parse a conversation kind from its stable storage name.
    ///
    /// Anything unrecognised (including a row written before the column existed)
    /// decodes as [`Self::Direct`], so an old history stays displayable.
    ///
    /// # Arguments
    ///
    /// * `value` - Name previously produced by [`ConversationKind::as_str`].
    ///
    /// # Returns
    ///
    /// Returns [`Self::Group`] for `"group"` and [`Self::Direct`] otherwise.
    /// Not `const`, although the body would allow it: the comparison uses
    /// `eq_ignore_ascii_case`, which is not a `const fn`, and spelling the check
    /// out as five byte comparisons would trade readability (and a second copy of
    /// the string) for a `const` no caller needs at compile time.
    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    pub fn from_db(value: &str) -> Self {
        if value.eq_ignore_ascii_case("group") {
            Self::Group
        } else {
            Self::Direct
        }
    }
}

/// A persisted chat message.
///
/// The record deliberately stores only what is needed to render a local
/// history: the direction, the conversation partner, the plaintext body, the
/// number of bytes that went over the wire, and whether the body was an ordinary
/// message or a third-person action (`/me`), so a restored history renders the
/// way it originally did.
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

    /// Ordinary message or third-person action.
    pub kind: MessageKind,

    /// How to interpret `body` (`binary` bodies are lowercase hexadecimal).
    pub content_type: ContentType,

    /// Whether this belongs to a direct conversation or a group.
    pub conversation: ConversationKind,

    /// Stable group identifier when `conversation` is
    /// [`ConversationKind::Group`], empty otherwise.
    pub group_id: String,

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
    /// * `kind` - Ordinary message or third-person action.
    /// * `content_type` - How to interpret `body`.
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
        kind: MessageKind,
        content_type: ContentType,
    ) -> Self {
        Self {
            id: 0,
            direction,
            peer: peer.into(),
            body: body.into(),
            wire_bytes,
            kind,
            content_type,
            conversation: ConversationKind::Direct,
            group_id: String::new(),
            created_at: Utc::now(),
        }
    }

    /// Create a record for one message of a group conversation.
    ///
    /// The group's **name** is stored as the peer, so a restored history renders
    /// the same `[group <name>]` label the live session showed, and its stable
    /// **id** is stored separately so a front-end can filter by group even after
    /// the group is renamed.
    ///
    /// # Arguments
    ///
    /// * `direction` - Sent or received.
    /// * `group` - The group the message belongs to.
    /// * `body` - Plaintext body (lowercase hexadecimal for a binary payload).
    /// * `wire_bytes` - Bytes that travelled over the wire.
    /// * `kind` - Ordinary message or third-person action.
    /// * `content_type` - How to interpret `body`.
    ///
    /// # Returns
    ///
    /// Returns a [`StoredMessage`] with an unassigned id (`0`).
    #[must_use]
    pub fn in_group(
        direction: MessageDirection,
        group: &crate::types::Group,
        body: impl Into<String>,
        wire_bytes: u64,
        kind: MessageKind,
        content_type: ContentType,
    ) -> Self {
        let name = if group.name.is_empty() {
            crate::utils::abbreviate(&group.id)
        } else {
            group.name.clone()
        };
        Self {
            id: 0,
            direction,
            peer: name,
            body: body.into(),
            wire_bytes,
            kind,
            content_type,
            conversation: ConversationKind::Group,
            group_id: group.id.clone(),
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
    ///
    /// `async` for the same reason: `CoreService` drives every manager through the
    /// same `new` → `start` → `shutdown` sequence, and a `SQLite` build has nothing
    /// to await here while a future remote backend will.
    #[allow(clippy::unused_async)]
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
    ///
    /// `async` by contract like [`DatabaseManager::new`], and here it is also
    /// feature-dependent: the `sqlite` build awaits opening the pool, and the
    /// driver-less build has nothing to await.
    #[allow(clippy::unused_async)]
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
                } else {
                    // `false` means "the schema is managed elsewhere", not "run without
                    // one". A store that was never initialised looks exactly like a
                    // prepared one until the first statement runs, and then every write
                    // fails with "no such table" for the rest of the session — so the
                    // mismatch is reported here, once, while nothing is running yet.
                    Self::require_schema(&pool).await?;
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
    ///
    /// `async` by contract like [`DatabaseManager::new`]; the `sqlite` build awaits
    /// the pool close, which is a real wait for in-flight queries.
    #[allow(clippy::unused_async)]
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
    /// Fail unless this build can use the database it was pointed at.
    ///
    /// Only called when migrations are disabled: with them on, the schema is created by
    /// [`DatabaseManager::run_migrations`] and cannot be missing. The tables and columns
    /// checked are exactly the ones the statements in this file bind, so a database that
    /// was initialised by an older build (a table without `group_id`) or never at all is
    /// refused with the name of what is missing, instead of accepting writes that cannot
    /// succeed.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] when a table or column this build writes to is
    /// absent.
    async fn require_schema(pool: &SqlitePool) -> MetaTextResult<()> {
        const SCHEMA: [(&str, &[&str]); 2] = [
            (
                "contacts",
                &[
                    "id",
                    "name",
                    "public_key",
                    "status",
                    "status_message",
                    "added_at",
                    "last_seen",
                    "avatar_hash",
                    "note",
                    "is_blocked",
                ],
            ),
            (
                "messages",
                &[
                    "id",
                    "direction",
                    "peer",
                    "body",
                    "wire_bytes",
                    "kind",
                    "content_type",
                    "conversation",
                    "group_id",
                    "created_at",
                ],
            ),
        ];

        for (table, columns) in SCHEMA {
            if !Self::sqlite_table_exists(pool, table).await? {
                return Err(Self::schema_error(&format!("no '{table}' table")));
            }
            for column in columns {
                if !Self::sqlite_column_exists(pool, table, column).await? {
                    return Err(Self::schema_error(&format!(
                        "{table} has no '{column}' column"
                    )));
                }
            }
        }

        Ok(())
    }

    /// The error for a database this build cannot use.
    fn schema_error(missing: &str) -> MetaTextError {
        MetaTextError::Database {
            message: format!(
                "the database has {missing} and migrations are disabled \
                 (database.enable_migrations = false): enable them, or point \
                 database.connection_string at a database whose schema is already applied"
            ),
            operation: "migrate".to_string(),
            source: None,
        }
    }

    /// Whether `table` exists in the database.
    ///
    /// `PRAGMA table_info` reports nothing for a table that does not exist, which is why
    /// the table itself is looked up in `sqlite_master` first.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] if the schema cannot be inspected.
    async fn sqlite_table_exists(pool: &SqlitePool, table: &str) -> MetaTextResult<bool> {
        let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_all(pool)
            .await
            .map_err(|error| MetaTextError::Database {
                message: format!("Failed to inspect the schema of {table}: {error}"),
                operation: "migrate".to_string(),
                source: None,
            })?;

        Ok(!rows.is_empty())
    }

    /// Create the schema used by metaText.
    ///
    /// The statements are idempotent (`IF NOT EXISTS`) so they are safe to run
    /// on every startup. A column added after the first release is applied with
    /// an explicit `ALTER TABLE`, guarded by a `PRAGMA table_info` check, so an
    /// existing database is upgraded in place instead of failing to open.
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
                kind TEXT NOT NULL DEFAULT 'text',
                content_type TEXT NOT NULL DEFAULT 'text',
                conversation TEXT NOT NULL DEFAULT 'direct',
                group_id TEXT NOT NULL DEFAULT '',
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

        // `messages.kind` and `messages.content_type` arrived after the first
        // release; a database created by that release has the table without them.
        // Each is added only when missing, so the same migration runs on a fresh
        // database, the first release's database, and one that already has both.
        for (column, definition) in [
            ("kind", "TEXT NOT NULL DEFAULT 'text'"),
            ("content_type", "TEXT NOT NULL DEFAULT 'text'"),
            ("conversation", "TEXT NOT NULL DEFAULT 'direct'"),
            ("group_id", "TEXT NOT NULL DEFAULT ''"),
        ] {
            if !Self::sqlite_column_exists(pool, "messages", column).await? {
                sqlx::query(&format!(
                    "ALTER TABLE messages ADD COLUMN {column} {definition}"
                ))
                .execute(pool)
                .await
                .map_err(|error| MetaTextError::Database {
                    message: format!("Failed to add messages.{column}: {error}"),
                    operation: "migrate".to_string(),
                    source: None,
                })?;
            }
        }

        Ok(())
    }

    /// Whether `table` already has a column named `column`.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Database`] if the schema cannot be inspected.
    async fn sqlite_column_exists(
        pool: &SqlitePool,
        table: &str,
        column: &str,
    ) -> MetaTextResult<bool> {
        // `PRAGMA table_info` cannot take a bound parameter for the table name, so
        // the name is validated instead: only a plain identifier is ever passed in.
        let statement = if table
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            format!("PRAGMA table_info({table})")
        } else {
            return Ok(false);
        };

        let rows = sqlx::query(&statement)
            .fetch_all(pool)
            .await
            .map_err(|error| MetaTextError::Database {
                message: format!("Failed to inspect the schema of {table}: {error}"),
                operation: "migrate".to_string(),
                source: None,
            })?;

        Ok(rows.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == column)
        }))
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
            "INSERT INTO messages (direction, peer, body, wire_bytes, kind, content_type,
                                   conversation, group_id, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(message.direction.as_str())
        .bind(&message.peer)
        .bind(&message.body)
        .bind(i64::try_from(message.wire_bytes).unwrap_or(i64::MAX))
        .bind(message.kind.as_str())
        .bind(message.content_type.as_str())
        .bind(message.conversation.as_str())
        .bind(&message.group_id)
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
            "SELECT id, direction, peer, body, wire_bytes, kind, content_type, conversation,
                    group_id, created_at
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
                // A row written before the column existed, or one with an unknown
                // value, decodes as an ordinary text message rather than failing.
                kind: MessageKind::from_db(row.try_get("kind").unwrap_or_default()),
                content_type: ContentType::from_db(row.try_get("content_type").unwrap_or_default()),
                // Same rule as the other columns: an old or damaged row decodes as
                // a direct conversation rather than failing the query.
                conversation: ConversationKind::from_db(
                    row.try_get("conversation").unwrap_or_default(),
                ),
                group_id: row.try_get("group_id").unwrap_or_default(),
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
        let message = StoredMessage::new(
            MessageDirection::Outgoing,
            "Alice",
            "hello",
            42,
            MessageKind::Action,
            ContentType::Text,
        );
        assert_eq!(message.id, 0);
        assert_eq!(message.direction, MessageDirection::Outgoing);
        assert_eq!(message.peer, "Alice");
        assert_eq!(message.body, "hello");
        assert_eq!(message.kind, MessageKind::Action);
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
                MessageKind::Text,
                ContentType::Text,
            ))
            .await
            .unwrap();
        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Incoming,
                "Alice",
                "second",
                20,
                MessageKind::Text,
                ContentType::Text,
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
    /// `enable_migrations = false` is only honoured for a database that has the schema.
    ///
    /// The key exists so the schema can be managed elsewhere, but it used to be trusted:
    /// a fresh file was opened, reported persistent through [`DatabaseManager::is_persistent`]
    /// and then failed every statement with "no such table" — a session that looked
    /// configured and stored nothing. The two halves below are the contract: a store whose
    /// schema is missing is refused at startup, and one whose schema was applied by an
    /// earlier run (the DBA case the key is for) opens without touching it.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_disabled_migrations_require_a_prepared_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed.db").to_string_lossy().to_string();

        let mut prepared = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string: path.clone(),
            max_connections: 1,
            enable_migrations: true,
        };

        // First run: the schema is created by the migrations.
        {
            let mut manager = DatabaseManager::new(&prepared).await.unwrap();
            manager.start().await.unwrap();
            assert!(manager.is_persistent());
            manager.shutdown().await.unwrap();
        }

        // Second run with migrations off: the schema is already there, so nothing is
        // created and nothing is missing.
        prepared.enable_migrations = false;
        {
            let mut manager = DatabaseManager::new(&prepared).await.unwrap();
            manager.start().await.unwrap();
            assert!(manager.is_persistent());
            manager
                .save_message(&StoredMessage::new(
                    MessageDirection::Outgoing,
                    "Alice",
                    "still writable",
                    14,
                    MessageKind::Text,
                    ContentType::Text,
                ))
                .await
                .unwrap();
            manager.shutdown().await.unwrap();
        }

        // A database that was never initialised is refused, with the missing table named.
        let fresh = dir.path().join("never-initialised.db");
        let mut uninitialised = prepared.clone();
        uninitialised.connection_string = fresh.to_string_lossy().to_string();

        let mut manager = DatabaseManager::new(&uninitialised).await.unwrap();
        let error = manager
            .start()
            .await
            .expect_err("a schema-less database must be refused");
        let message = error.to_string();
        assert!(
            message.contains("no 'contacts' table") && message.contains("enable_migrations"),
            "{message}"
        );
        assert!(fresh.exists(), "the file itself is created by SQLite");
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
                MessageKind::Text,
                ContentType::Text,
            ))
            .await
            .unwrap();
        assert_eq!(manager.message_count().await.unwrap(), 1);

        manager.shutdown().await.unwrap();
    }

    /// Message kinds survive a save/load cycle, and a message and an action with
    /// the same body stay distinguishable in the history.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_message_kind_round_trips_through_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let config = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string: dir.path().join("kind.db").to_string_lossy().to_string(),
            max_connections: 1,
            enable_migrations: true,
        };

        let mut manager = DatabaseManager::new(&config).await.unwrap();
        manager.start().await.unwrap();
        assert!(manager.is_persistent());

        for kind in [MessageKind::Text, MessageKind::Action] {
            manager
                .save_message(&StoredMessage::new(
                    MessageDirection::Outgoing,
                    "Alice",
                    "waves",
                    7,
                    kind,
                    ContentType::Text,
                ))
                .await
                .unwrap();
        }

        // A binary body is stored as hexadecimal with its content type, so a
        // restored history knows not to render it as text.
        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Outgoing,
                "Alice",
                "00ff10",
                3,
                MessageKind::Text,
                ContentType::Binary,
            ))
            .await
            .unwrap();

        let history = manager.recent_messages(10).await.unwrap();
        assert_eq!(history.len(), 3);
        // Chronological order is preserved: text, action, then binary.
        assert_eq!(history[0].kind, MessageKind::Text);
        assert_eq!(history[1].kind, MessageKind::Action);
        assert_eq!(history[0].body, history[1].body);
        assert_eq!(history[2].content_type, ContentType::Binary);
        assert_eq!(history[2].body, "00ff10");
        assert_eq!(history[0].content_type, ContentType::Text);

        manager.shutdown().await.unwrap();
    }

    /// A group message round-trips with its conversation kind and group id.
    ///
    /// Needs a real driver: without the `sqlite` feature the store refuses to
    /// persist at all, which is a different thing from "the round trip is
    /// broken".
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_group_message_round_trips_through_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let config = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string: dir.path().join("group.db").to_string_lossy().to_string(),
            max_connections: 1,
            enable_migrations: true,
        };

        let mut manager = DatabaseManager::new(&config).await.unwrap();
        manager.start().await.unwrap();

        let group = crate::types::Group {
            id: "ab".repeat(32),
            name: "Team".to_string(),
            members: 2,
            joined: true,
        };
        manager
            .save_message(&StoredMessage::in_group(
                MessageDirection::Incoming,
                &group,
                "hello group",
                11,
                MessageKind::Action,
                ContentType::Text,
            ))
            .await
            .unwrap();

        // A direct message is still a direct message, even after a group one.
        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Outgoing,
                "Alice",
                "hi",
                2,
                MessageKind::Text,
                ContentType::Text,
            ))
            .await
            .unwrap();

        let history = manager.recent_messages(10).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].conversation, ConversationKind::Group);
        assert_eq!(history[0].group_id, group.id);
        assert_eq!(history[0].peer, "Team", "the group name labels the row");
        assert_eq!(history[0].body, "hello group");
        assert_eq!(history[0].kind, MessageKind::Action);
        assert_eq!(history[1].conversation, ConversationKind::Direct);
        assert!(history[1].group_id.is_empty());
        assert_eq!(history[1].peer, "Alice");

        // An unnamed group is still identifiable by its abbreviated id.
        let unnamed = crate::types::Group {
            id: "cd".repeat(32),
            name: String::new(),
            members: 1,
            joined: true,
        };
        let record = StoredMessage::in_group(
            MessageDirection::Incoming,
            &unnamed,
            "x",
            1,
            MessageKind::Text,
            ContentType::Text,
        );
        assert_eq!(record.conversation, ConversationKind::Group);
        assert_eq!(record.group_id, unnamed.id);
        assert!(
            record.peer.contains('…'),
            "an unnamed group is labelled with an abbreviated id: {record:?}"
        );

        manager.shutdown().await.unwrap();
    }

    /// A database created before `messages.kind` existed is upgraded in place:
    /// the column is added and old rows read back as ordinary messages instead
    /// of failing the whole history query.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_migration_adds_the_kind_column_to_a_legacy_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let connection_string = path.to_string_lossy().to_string();

        // Build the pre-`kind` schema by hand, exactly as the first release left it.
        {
            let options = SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options)
                .await
                .unwrap();
            sqlx::query(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    direction TEXT NOT NULL,
                    peer TEXT NOT NULL,
                    body TEXT NOT NULL,
                    wire_bytes INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL
                )",
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO messages (direction, peer, body, wire_bytes, created_at)
                 VALUES ('in', 'Alice', 'written before the column existed', 9,
                         '2024-01-01T00:00:00+00:00')",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }

        let config = DatabaseConfig {
            database_type: "sqlite".to_string(),
            connection_string,
            max_connections: 1,
            enable_migrations: true,
        };
        let mut manager = DatabaseManager::new(&config).await.unwrap();
        manager
            .start()
            .await
            .expect("the migration must add the missing column");

        let history = manager.recent_messages(10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].body, "written before the column existed");
        assert_eq!(
            history[0].kind,
            MessageKind::Text,
            "a legacy row must decode as an ordinary message"
        );
        assert_eq!(
            history[0].content_type,
            ContentType::Text,
            "a legacy row must decode as text"
        );
        assert_eq!(
            history[0].conversation,
            ConversationKind::Direct,
            "a legacy row must decode as a direct conversation"
        );
        assert!(
            history[0].group_id.is_empty(),
            "a legacy row belongs to no group"
        );

        // The upgraded table accepts the new columns on the next write.
        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Incoming,
                "Alice",
                "after the upgrade",
                6,
                MessageKind::Action,
                ContentType::Text,
            ))
            .await
            .unwrap();
        manager
            .save_message(&StoredMessage::new(
                MessageDirection::Incoming,
                "Alice",
                "00ff",
                2,
                MessageKind::Text,
                ContentType::Binary,
            ))
            .await
            .unwrap();
        let history = manager.recent_messages(10).await.unwrap();
        assert_eq!(history[1].kind, MessageKind::Action);
        assert_eq!(history[2].content_type, ContentType::Binary);

        manager.shutdown().await.unwrap();
    }
}
