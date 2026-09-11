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

    /// Validate configuration settings
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if configuration is valid, or a list of validation errors.
    pub fn validate(&self) -> MetaTextResult<()> {
        let mut errors = Vec::new();

        // Validate app settings
        if self.app.name.is_empty() {
            errors.push("Application name cannot be empty".to_string());
        }
        if self.app.max_friends == 0 {
            errors.push("Maximum friends must be greater than 0".to_string());
        }
        if self.app.max_message_length == 0 {
            errors.push("Maximum message length must be greater than 0".to_string());
        }

        // Validate network settings
        if self.network.port == 0 {
            errors.push("Network port cannot be 0".to_string());
        }
        if self.network.max_connections == 0 {
            errors.push("Maximum connections must be greater than 0".to_string());
        }

        // Validate database settings
        if self.database.connection_string.is_empty() {
            errors.push("Database connection string cannot be empty".to_string());
        }

        if !errors.is_empty() {
            return Err(MetaTextError::Configuration {
                message: format!("Configuration validation failed: {}", errors.join(", ")),
                source: None,
            });
        }

        Ok(())
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
}
