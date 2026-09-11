/*!
 * config.rs
 *
 * Configuration management for metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - TOML-based configuration files
 * - Environment variable overrides
 * - Default configuration generation
 * - Configuration validation
 */

use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::{info, warn};

use crate::error::{MetaTextError, MetaTextResult};

/// Largest message length a peer can actually deliver.
///
/// The transport frame limit is 64 KiB and the ciphertext adds a nonce and a
/// tag, so a configuration asking for more would be physically undeliverable.
pub const MAX_MESSAGE_LENGTH: usize = 32 * 1024;

/// Longest accepted auto-save interval, in seconds (24 hours).
pub const MAX_AUTO_SAVE_INTERVAL: u64 = 24 * 60 * 60;

/// Longest accepted outbound connection timeout, in seconds (one hour).
pub const MAX_CONNECTION_TIMEOUT: u64 = 60 * 60;

/// Database backends this build understands.
pub const SUPPORTED_DATABASES: [&str; 3] = ["sqlite", "postgres", "mysql"];

/// Encryption algorithms this build understands.
pub const SUPPORTED_ALGORITHMS: [&str; 1] = ["ChaCha20-Poly1305"];

/// Key derivation functions this build understands.
pub const SUPPORTED_KDFS: [&str; 1] = ["Argon2"];

/// Main application configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Application general settings
    pub app: AppSettings,

    /// Network configuration
    pub network: NetworkConfig,

    /// Database configuration
    pub database: DatabaseConfig,

    /// Cryptographic settings
    pub crypto: CryptoConfig,

    /// User interface settings
    pub ui: UiConfig,

    /// Logging configuration
    pub logging: LoggingConfig,
}

/// Application general settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    /// Application name
    pub name: String,

    /// Application version
    pub version: String,

    /// Default user nickname
    pub default_nickname: String,

    /// Default status message
    pub default_status: String,

    /// Maximum number of friends
    pub max_friends: usize,

    /// Maximum message length
    pub max_message_length: usize,

    /// Auto-save interval in seconds
    pub auto_save_interval: u64,
}

/// Network configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Default network port
    pub port: u16,

    /// Bootstrap nodes list
    pub bootstrap_nodes: Vec<String>,

    /// Connection timeout in seconds
    pub connection_timeout: u64,

    /// Maximum connections
    pub max_connections: u32,

    /// Enable UPnP port forwarding
    pub enable_upnp: bool,

    /// Enable IPv6
    pub enable_ipv6: bool,
}

/// Database configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Database type (sqlite, postgres, mysql)
    pub database_type: String,

    /// Database connection string
    pub connection_string: String,

    /// Maximum database connections
    pub max_connections: u32,

    /// Enable database migrations
    pub enable_migrations: bool,
}

/// Cryptographic configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoConfig {
    /// Enable encryption by default
    pub enable_encryption: bool,

    /// Encryption algorithm
    pub algorithm: String,

    /// Key derivation function
    pub kdf: String,

    /// Key rotation interval in days
    pub key_rotation_days: u32,

    /// Enable perfect forward secrecy
    pub enable_pfs: bool,
}

/// User interface configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Default theme
    pub theme: String,

    /// Enable colors
    pub enable_colors: bool,

    /// Enable mouse support
    pub enable_mouse: bool,

    /// Message display format
    pub message_format: String,

    /// Auto-scroll to new messages
    pub auto_scroll: bool,
}

/// Logging configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level
    pub level: String,

    /// Log file path
    pub file_path: String,

    /// Enable console logging
    pub enable_console: bool,

    /// Enable file logging
    pub enable_file: bool,

    /// Log rotation size in MB
    pub rotation_size_mb: u64,

    /// Maximum log files to keep
    pub max_files: usize,
}

impl AppConfig {
    /// Load configuration from file
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the configuration file
    ///
    /// # Returns
    ///
    /// Returns the loaded configuration or an error if loading fails.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::config::AppConfig;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let config = AppConfig::load("config.toml").await?;
    ///     println!("Loaded config: {:?}", config);
    ///     Ok(())
    /// }
    /// ```
    pub async fn load<P: AsRef<Path>>(path: P) -> MetaTextResult<Self> {
        let path = path.as_ref();
        let timestamp = chrono::Utc::now();

        info!(
            "📋 [{}] Loading configuration from: {}",
            timestamp.format("%Y-%m-%d %H:%M:%S"),
            path.display()
        );

        if !path.exists() {
            warn!(
                "⚠️ [{}] Configuration file not found, creating default",
                timestamp.format("%Y-%m-%d %H:%M:%S")
            );
            let config = Self::default();
            config.save(path).await?;
            return Ok(config);
        }

        let content =
            tokio::fs::read_to_string(path)
                .await
                .map_err(|e| MetaTextError::Configuration {
                    message: format!("Failed to read config file: {}", e),
                    source: Some(Box::new(e)),
                })?;

        let config: AppConfig =
            toml::from_str(&content).map_err(|e| MetaTextError::Configuration {
                message: format!("Failed to parse config file: {}", e),
                source: Some(Box::new(e)),
            })?;

        info!(
            "✅ [{}] Configuration loaded successfully",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );
        Ok(config)
    }

    /// Save configuration to file
    ///
    /// # Arguments
    ///
    /// * `path` - Path where to save the configuration file
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if saving succeeds, or an error if it fails.
    pub async fn save<P: AsRef<Path>>(&self, path: P) -> MetaTextResult<()> {
        let path = path.as_ref();
        let timestamp = chrono::Utc::now();

        info!(
            "💾 [{}] Saving configuration to: {}",
            timestamp.format("%Y-%m-%d %H:%M:%S"),
            path.display()
        );

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| MetaTextError::Configuration {
                    message: format!("Failed to create config directory: {}", e),
                    source: Some(Box::new(e)),
                })?;
        }

        let content = toml::to_string_pretty(self).map_err(|e| MetaTextError::Configuration {
            message: format!("Failed to serialize config: {}", e),
            source: Some(Box::new(e)),
        })?;

        tokio::fs::write(path, content)
            .await
            .map_err(|e| MetaTextError::Configuration {
                message: format!("Failed to write config file: {}", e),
                source: Some(Box::new(e)),
            })?;

        info!(
            "✅ [{}] Configuration saved successfully",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        );
        Ok(())
    }

    /// Collect every configuration problem.
    ///
    /// The list is returned in full instead of stopping at the first problem so
    /// an operator can fix a configuration file in one pass.
    ///
    /// # Returns
    ///
    /// Returns every problem found; an empty vector means the configuration is
    /// consistent and usable.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::config::AppConfig;
    ///
    /// assert!(AppConfig::default().problems().is_empty());
    ///
    /// let mut broken = AppConfig::default();
    /// broken.network.port = 0;
    /// assert_eq!(broken.problems().len(), 1);
    /// ```
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut errors = Vec::new();

        // -- [app] ----------------------------------------------------------
        if self.app.name.trim().is_empty() {
            errors.push("app.name cannot be empty".to_string());
        }
        if self.app.version.trim().is_empty() {
            errors.push("app.version cannot be empty".to_string());
        }
        if self.app.max_friends == 0 {
            errors.push("app.max_friends must be greater than 0".to_string());
        }
        if self.app.max_message_length == 0 {
            errors.push("app.max_message_length must be greater than 0".to_string());
        }
        // A message is framed and encrypted; anything beyond the transport
        // frame limit could never be delivered anyway.
        if self.app.max_message_length > MAX_MESSAGE_LENGTH {
            errors.push(format!(
                "app.max_message_length must be at most {MAX_MESSAGE_LENGTH}"
            ));
        }
        if self.app.auto_save_interval > MAX_AUTO_SAVE_INTERVAL {
            errors.push(format!(
                "app.auto_save_interval must be at most {MAX_AUTO_SAVE_INTERVAL} seconds"
            ));
        }

        // -- [network] ------------------------------------------------------
        if self.network.port == 0 {
            errors.push("network.port cannot be 0".to_string());
        }
        if self.network.max_connections == 0 {
            errors.push("network.max_connections must be greater than 0".to_string());
        }
        if self.network.connection_timeout == 0 {
            errors.push("network.connection_timeout must be greater than 0".to_string());
        }
        if self.network.connection_timeout > MAX_CONNECTION_TIMEOUT {
            errors.push(format!(
                "network.connection_timeout must be at most {MAX_CONNECTION_TIMEOUT} seconds"
            ));
        }
        for node in &self.network.bootstrap_nodes {
            if !crate::utils::is_valid_host_port(node) {
                errors.push(format!(
                    "network.bootstrap_nodes entry is not a host:port address: {node}"
                ));
            }
        }

        // -- [database] -----------------------------------------------------
        if self.database.connection_string.trim().is_empty() {
            errors.push("database.connection_string cannot be empty".to_string());
        }
        if self.database.max_connections == 0 {
            errors.push("database.max_connections must be greater than 0".to_string());
        }
        if !SUPPORTED_DATABASES
            .iter()
            .any(|kind| kind.eq_ignore_ascii_case(self.database.database_type.trim()))
        {
            errors.push(format!(
                "database.database_type must be one of {SUPPORTED_DATABASES:?}, found '{}'",
                self.database.database_type
            ));
        }

        // -- [crypto] -------------------------------------------------------
        if !SUPPORTED_ALGORITHMS
            .iter()
            .any(|name| name.eq_ignore_ascii_case(self.crypto.algorithm.trim()))
        {
            errors.push(format!(
                "crypto.algorithm must be one of {SUPPORTED_ALGORITHMS:?}, found '{}'",
                self.crypto.algorithm
            ));
        }
        if !SUPPORTED_KDFS
            .iter()
            .any(|name| name.eq_ignore_ascii_case(self.crypto.kdf.trim()))
        {
            errors.push(format!(
                "crypto.kdf must be one of {SUPPORTED_KDFS:?}, found '{}'",
                self.crypto.kdf
            ));
        }

        // -- [ui] -----------------------------------------------------------
        if self.ui.theme.trim().is_empty() {
            errors.push("ui.theme cannot be empty".to_string());
        }
        if self.ui.message_format.trim().is_empty() {
            errors.push("ui.message_format cannot be empty".to_string());
        }

        // -- [logging] ------------------------------------------------------
        if crate::cli::LogLevel::parse(self.logging.level.trim()).is_none() {
            errors.push(format!(
                "logging.level must be one of error, warn, info, debug, trace; found '{}'",
                self.logging.level
            ));
        }
        if self.logging.enable_file && self.logging.file_path.trim().is_empty() {
            errors
                .push("logging.file_path cannot be empty when file logging is enabled".to_string());
        }
        if self.logging.rotation_size_mb == 0 {
            errors.push("logging.rotation_size_mb must be greater than 0".to_string());
        }
        if self.logging.max_files == 0 {
            errors.push("logging.max_files must be greater than 0".to_string());
        }

        errors
    }

    /// Validate configuration settings
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if configuration is valid, or an error listing every
    /// problem found.
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Configuration`] when [`AppConfig::problems`] is
    /// not empty.
    pub fn validate(&self) -> MetaTextResult<()> {
        let errors = self.problems();
        if errors.is_empty() {
            return Ok(());
        }

        Err(MetaTextError::Configuration {
            message: format!("Configuration validation failed: {}", errors.join(", ")),
            source: None,
        })
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            name: "metaText".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            default_nickname: "metaText00".to_string(),
            default_status: "Keep on Metaverse .......".to_string(),
            max_friends: 1024,
            max_message_length: 1372,
            auto_save_interval: 300, // 5 minutes
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            port: 33445,
            bootstrap_nodes: vec![
                "tox.abilinski.com:33445".to_string(),
                "tox.abilinski.com:3389".to_string(),
                "tox.instinctive.ch:33445".to_string(),
                "tox.instinctive.ch:3389".to_string(),
            ],
            connection_timeout: 30,
            max_connections: 100,
            enable_upnp: true,
            enable_ipv6: true,
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            database_type: "sqlite".to_string(),
            connection_string: "meta_text.db".to_string(),
            max_connections: 10,
            enable_migrations: true,
        }
    }
}

impl Default for CryptoConfig {
    fn default() -> Self {
        Self {
            enable_encryption: true,
            algorithm: "ChaCha20-Poly1305".to_string(),
            kdf: "Argon2".to_string(),
            key_rotation_days: 30,
            enable_pfs: true,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            enable_colors: true,
            enable_mouse: true,
            message_format: "[{time}] {sender}: {message}".to_string(),
            auto_scroll: true,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            file_path: "logs/meta-text.log".to_string(),
            enable_console: true,
            enable_file: true,
            rotation_size_mb: 10,
            max_files: 5,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app: AppSettings::default(),
            network: NetworkConfig::default(),
            database: DatabaseConfig::default(),
            crypto: CryptoConfig::default(),
            ui: UiConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_config_default() {
        let config = AppConfig::default();

        assert_eq!(config.app.name, "metaText");
        assert_eq!(config.app.max_friends, 1024);
        assert_eq!(config.network.port, 33445);
        assert!(config.crypto.enable_encryption);
    }

    #[tokio::test]
    async fn test_config_save_and_load() {
        let config = AppConfig::default();
        let temp_file = NamedTempFile::new().unwrap();

        // Save config
        config.save(&temp_file.path()).await.unwrap();

        // Load config
        let loaded_config = AppConfig::load(&temp_file.path()).await.unwrap();

        assert_eq!(config.app.name, loaded_config.app.name);
        assert_eq!(config.network.port, loaded_config.network.port);
    }

    #[test]
    fn test_config_validation() {
        let mut config = AppConfig::default();

        // Valid config should pass
        assert!(config.validate().is_ok());

        // Invalid config should fail
        config.app.name = String::new();
        assert!(config.validate().is_err());
    }

    /// The default configuration ships clean.
    #[test]
    fn test_default_configuration_has_no_problems() {
        assert_eq!(AppConfig::default().problems(), Vec::<String>::new());
    }

    /// Every field with a limit is actually checked.
    #[test]
    fn test_problems_cover_every_section() {
        type Mutate = fn(&mut AppConfig);
        let cases: Vec<(&str, Mutate)> = vec![
            ("app.name", |c| c.app.name = "  ".to_string()),
            ("app.version", |c| c.app.version = String::new()),
            ("app.max_friends", |c| c.app.max_friends = 0),
            ("app.max_message_length", |c| c.app.max_message_length = 0),
            ("app.max_message_length (upper)", |c| {
                c.app.max_message_length = MAX_MESSAGE_LENGTH + 1;
            }),
            ("app.auto_save_interval", |c| {
                c.app.auto_save_interval = MAX_AUTO_SAVE_INTERVAL + 1;
            }),
            ("network.port", |c| c.network.port = 0),
            ("network.max_connections", |c| c.network.max_connections = 0),
            ("network.connection_timeout (zero)", |c| {
                c.network.connection_timeout = 0;
            }),
            ("network.connection_timeout (upper)", |c| {
                c.network.connection_timeout = MAX_CONNECTION_TIMEOUT + 1;
            }),
            ("network.bootstrap_nodes", |c| {
                c.network.bootstrap_nodes = vec!["not-an-address".to_string()];
            }),
            ("database.connection_string", |c| {
                c.database.connection_string = "   ".to_string();
            }),
            ("database.database_type", |c| {
                c.database.database_type = "mongodb".to_string();
            }),
            ("database.max_connections", |c| {
                c.database.max_connections = 0;
            }),
            ("crypto.algorithm", |c| {
                c.crypto.algorithm = "ROT13".to_string()
            }),
            ("crypto.kdf", |c| c.crypto.kdf = "md5".to_string()),
            ("ui.theme", |c| c.ui.theme = String::new()),
            ("ui.message_format", |c| c.ui.message_format = String::new()),
            ("logging.level", |c| c.logging.level = "verbose".to_string()),
            ("logging.file_path", |c| {
                c.logging.enable_file = true;
                c.logging.file_path = "  ".to_string();
            }),
            ("logging.rotation_size_mb", |c| {
                c.logging.rotation_size_mb = 0
            }),
            ("logging.max_files", |c| c.logging.max_files = 0),
        ];

        for (field, mutate) in cases {
            let mut config = AppConfig::default();
            mutate(&mut config);
            let problems = config.problems();
            assert!(
                !problems.is_empty(),
                "{field} should have been rejected but was accepted"
            );
            assert!(
                config.validate().is_err(),
                "{field} should make validate() fail"
            );
        }
    }

    /// Every problem is reported, not just the first one.
    #[test]
    fn test_problems_are_reported_together() {
        let mut config = AppConfig::default();
        config.app.name = String::new();
        config.network.port = 0;
        config.crypto.kdf = "nope".to_string();

        let problems = config.problems();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("app.name")));
        assert!(problems.iter().any(|p| p.contains("network.port")));
        assert!(problems.iter().any(|p| p.contains("crypto.kdf")));
    }

    /// File logging is only required to have a path when it is enabled.
    #[test]
    fn test_logging_file_path_only_required_when_enabled() {
        let mut config = AppConfig::default();
        config.logging.enable_file = false;
        config.logging.file_path = String::new();
        assert!(config.validate().is_ok());

        config.logging.enable_file = true;
        assert!(config.validate().is_err());
    }
}
