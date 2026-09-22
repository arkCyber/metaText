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

/// Default keepalive cadence, in seconds.
///
/// The transport sends one empty `FRAME_KEEPALIVE` for each greeted connection on this
/// cadence — `network::KEEPALIVE_INTERVAL` is defined *from* this constant, so the
/// default can only ever be one number — and it is also the **slowest** cadence an
/// operator may configure, see [`NetworkConfig::keepalive_interval`].
pub const DEFAULT_KEEPALIVE_INTERVAL: u64 = 20;

/// Fastest accepted keepalive cadence, in seconds.
///
/// A shorter cadence costs five bytes per connection per interval, so one second is
/// generous rather than restrictive. What the floor rules out is `0`: "never probe"
/// would reopen the half-open hole `docs/ARCHITECTURE.md` §6.20 closed, which is why
/// the key is bounded at both ends instead of being a free-form number.
pub const MIN_KEEPALIVE_INTERVAL: u64 = 1;

/// `serde` default for [`NetworkConfig::keepalive_interval`].
///
/// A configuration file written while the cadence was a transport constant has no such
/// key, and must keep loading with the cadence it had — which is exactly what the
/// constant was. Without this a previously valid file would stop the process at
/// startup.
const fn default_keepalive_interval() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL
}

/// Database backends this build understands.
pub const SUPPORTED_DATABASES: [&str; 3] = ["sqlite", "postgres", "mysql"];

/// Encryption algorithms this build understands.
pub const SUPPORTED_ALGORITHMS: [&str; 1] = ["ChaCha20-Poly1305"];

/// Key derivation functions this build understands.
pub const SUPPORTED_KDFS: [&str; 1] = ["Argon2"];

/// Themes the full screen interface can draw (`[ui] theme`).
pub const SUPPORTED_THEMES: [&str; 2] = ["dark", "light"];

/// The one `[ui] message_format` the interface keeps.
///
/// Message lines are rendered by the presentation layer, which adds the direction,
/// the peer and the delivery report around the body; a template would have to
/// describe that whole line, and the placeholder set for it does not exist yet. A
/// value other than this one is therefore refused instead of being accepted and
/// ignored, and the shipped default stays loadable.
pub const DEFAULT_MESSAGE_FORMAT: &str = "[{time}] {sender}: {message}";

/// Highest `ipc.requests_per_second` the configuration accepts.
///
/// A value this large already means "no practical limit"; the bound exists so a
/// typo (`256000000`) is a startup error rather than a number nobody can reason
/// about. `0` is the documented way to disable the limiter.
pub const MAX_IPC_REQUESTS_PER_SECOND: u32 = 100_000;

/// Highest `ipc.request_burst` the configuration accepts.
pub const MAX_IPC_REQUEST_BURST: u32 = 1_000_000;

/// Main application configuration structure
///
/// `Default` is derived: every section has a default of its own, so the
/// application's default is exactly the sum of them. The manual `impl` this
/// replaced listed the sections in the same order and could drift from the
/// struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Core protocol endpoint settings (`[ipc]`)
    ///
    /// Defaulted rather than required so a configuration file written before this
    /// section existed still loads: every other section is mandatory, and adding a
    /// mandatory one would refuse an operator's existing file.
    #[serde(default)]
    pub ipc: IpcConfig,
}

/// Settings for the out-of-process core endpoint (`[ipc]`).
///
/// The endpoint itself is started from the command line (`--ipc-listen` /
/// `--ipc-token`); what lives here is the *policy* it enforces, which the flags
/// `--ipc-rate` / `--ipc-burst` override for one run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcConfig {
    /// Sustained requests per second one client **address** may use (`0` disables
    /// the limit).
    ///
    /// The budget belongs to the address rather than to the connection, so a client
    /// cannot reset it by reconnecting. Two front-ends on one host therefore share
    /// it, which is why the default is far above what an interactive interface
    /// needs.
    pub requests_per_second: u32,

    /// Burst one client address may spend above the sustained rate.
    pub request_burst: u32,
}

/// Default sustained request rate one client **address** may use, per second.
///
/// `0` disables the limit. The value is far above what an interactive front-end
/// needs, so it only ever bites a misbehaving or hostile client. It lives here
/// (rather than in the endpoint that enforces it) because it is a *default for a
/// configuration value*: the subsystem owns it, the service only reads it. The
/// endpoint re-exports it, so `ipc::server::DEFAULT_REQUESTS_PER_SECOND` keeps
/// naming the same number.
pub const DEFAULT_REQUESTS_PER_SECOND: u32 = 256;

/// Default burst one client address may spend above the sustained rate.
pub const DEFAULT_REQUEST_BURST: u32 = 512;

impl Default for IpcConfig {
    fn default() -> Self {
        // The endpoint applies the same constants when it is constructed without
        // an explicit policy, so a configuration file without an `[ipc]` section
        // and a programmatic `ServerOptions::default()` agree.
        Self {
            requests_per_second: DEFAULT_REQUESTS_PER_SECOND,
            request_burst: DEFAULT_REQUEST_BURST,
        }
    }
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
    /// Network port; `0` asks the OS for a free one
    pub port: u16,

    /// Bootstrap nodes list
    pub bootstrap_nodes: Vec<String>,

    /// Connection timeout in seconds.
    ///
    /// One knob, two deadlines, because both answer "how long may this connection
    /// stay useless": how long a single outbound dial attempt may take, and how long
    /// a peer that has connected has to announce itself before its slot is
    /// reclaimed. Without the second one a socket that connects and says nothing
    /// occupies a connection slot forever.
    pub connection_timeout: u64,

    /// Maximum connections
    pub max_connections: u32,

    /// How often an empty keepalive frame is sent to each greeted peer, in seconds.
    ///
    /// The knob is deliberately **one-way**: it may shorten the cadence below
    /// [`DEFAULT_KEEPALIVE_INTERVAL`], never raise it above it.
    ///
    /// - Shortening it is safe and is what the key exists for: a NAT that forgets an
    ///   idle mapping sooner than 20 s needs traffic more often, and a faster probe is
    ///   compatible with every peer, because a shorter cadence only produces *more*
    ///   evidence that this instance is alive.
    /// - Raising it is not, and [`AppConfig::problems`] refuses it: the idle deadline
    ///   is not derived from this value (see [`crate::network::NetworkManager`]), so a
    ///   peer enforcing the default deadline — three default cadences — would drop this
    ///   instance for silence. Two ends that disagree about the cadence in that
    ///   direction tear the connection down; that is the mismatch the transport
    ///   constant used to rule out, and the validation rule keeps ruling it out.
    ///
    /// The *deadline* itself is not configurable, so slowing the probe can never be
    /// mistaken for "the peer may now be quiet for longer". Defaulted so a
    /// configuration file written before the key existed still loads.
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,

    /// Whether to ask the gateway for a `UPnP` port mapping.
    ///
    /// **Not implemented**: no mapping client is linked, so `true` is rejected by
    /// [`AppConfig::problems`] rather than silently ignored — a user who asked for
    /// a forwarded port must not be left believing it happened.
    pub enable_upnp: bool,

    /// Enable IPv6
    ///
    /// Enforced: the TCP listener binds a dual-stack IPv6 wildcard and the Tox
    /// transport passes it to toxcore's `ipv6_enabled`, so this key is not a
    /// promise the binary does not keep.
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
    ///
    /// Honoured by [`crate::database::DatabaseManager`]: with `true` the schema is
    /// created (and upgraded in place) when the store is opened, with `false` it is left
    /// to somebody else. `false` therefore means "the schema is managed elsewhere", and
    /// a store whose schema is missing is refused at startup with the missing table
    /// named — the alternative was a session that opens, reports itself persistent, and
    /// fails every write.
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

    /// Key rotation interval in days.
    ///
    /// **Not implemented, and not implementable as a local knob.** The session key comes
    /// from `--passphrase` and is shared with every peer that has it, and the identity
    /// key is the X25519 pair peers pin: rotating either on a local timer would leave the
    /// other end unable to read what this one writes, so there is no rotation the binary
    /// could honestly perform. `0` — "no rotation" — is the only accepted value, and
    /// anything else is refused by [`AppConfig::problems`] instead of being accepted and
    /// ignored.
    pub key_rotation_days: u32,

    /// Enable perfect forward secrecy.
    ///
    /// The per-connection agreement ([`crate::identity::pair_key`]) is **not optional**:
    /// it is what the TCP transport's identity handshake derives the pair key with, so
    /// there is no downgrade path a `false` could select. It is therefore refused by
    /// [`AppConfig::problems`] rather than accepted and ignored — the failure mode
    /// `network.enable_upnp` is refused for. `true` describes what that transport does;
    /// the Tox transport carries neither identity nor ephemeral frames, and leaves
    /// payload encryption to toxcore.
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
    ///
    /// A file is rolled over once it reaches this size, *and* when the day changes:
    /// the day's first file is `{file_path}.YYYY-MM-DD`, its continuations are
    /// `{file_path}.YYYY-MM-DD.001`, `.002`, … Enforced by
    /// [`crate::logging::SizeRotatingWriter`].
    pub rotation_size_mb: u64,

    /// Maximum number of log files to keep
    ///
    /// The active file counts towards the limit, and the oldest files are removed
    /// first, so `5` means "this file plus four older ones".
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
    /// use meta_text_backend::config::AppConfig;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     // A throwaway directory on purpose: `load` writes the defaults when the
    ///     // file is missing, and a documentation example must not leave a
    ///     // `config.toml` behind in whatever directory it happens to run in.
    ///     let dir = tempfile::tempdir()?;
    ///     let path = dir.path().join("config.toml");
    ///
    ///     let config = AppConfig::load(&path).await?;
    ///     println!("Loaded config: {:?}", config);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Configuration`] when the file exists but cannot be
    /// read or does not parse. A file that does not exist is *not* an error: the
    /// defaults are written to `path` and returned, which is what makes a first
    /// start work without a configuration file.
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
                    message: format!("Failed to read config file: {e}"),
                    source: Some(Box::new(e)),
                })?;

        let config: Self = toml::from_str(&content).map_err(|e| MetaTextError::Configuration {
            message: format!("Failed to parse config file: {e}"),
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
    ///
    /// # Errors
    ///
    /// Returns [`MetaTextError::Configuration`] when a parent directory cannot be
    /// created, the serialised form cannot be written, or the atomic replace
    /// fails. The write goes to a sibling `.tmp` file that is renamed over the
    /// target, so an interrupted save cannot truncate the existing configuration.
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
                    message: format!("Failed to create config directory: {e}"),
                    source: Some(Box::new(e)),
                })?;
        }

        let content = toml::to_string_pretty(self).map_err(|e| MetaTextError::Configuration {
            message: format!("Failed to serialize config: {e}"),
            source: Some(Box::new(e)),
        })?;

        // Write a sibling file and rename it over the target: `rename` is atomic
        // on the same filesystem, so an interrupted save cannot truncate the
        // configuration an operator depends on.
        let temporary = path.with_extension("tmp");
        tokio::fs::write(&temporary, content)
            .await
            .map_err(|e| MetaTextError::Configuration {
                message: format!("Failed to write config file: {e}"),
                source: Some(Box::new(e)),
            })?;
        tokio::fs::rename(&temporary, path)
            .await
            .map_err(|e| MetaTextError::Configuration {
                message: format!("Failed to replace config file: {e}"),
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
    /// use meta_text_backend::config::AppConfig;
    ///
    /// assert!(AppConfig::default().problems().is_empty());
    ///
    /// let mut broken = AppConfig::default();
    /// broken.app.max_friends = 0;
    /// assert_eq!(broken.problems().len(), 1);
    /// ```
    #[must_use]
    #[allow(clippy::too_many_lines)]
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
        // `port == 0` is valid on purpose: it means "let the OS pick a free
        // port", which is how two instances (and every integration test) run
        // without hand-picked numbers. The bound port is what `/peers` reports.
        if self.network.max_connections == 0 {
            errors.push("network.max_connections must be greater than 0".to_string());
        }
        // `enable_upnp` is accepted by the parser but no mapping client is linked,
        // so asking for it must fail loudly instead of quietly not happening — the
        // same "accepted and ignored" failure A14 removed for the rest of the file.
        if self.network.enable_upnp {
            errors.push(
                "network.enable_upnp is not implemented (no UPnP mapping client is \
                 linked); set it to false or remove the key"
                    .to_string(),
            );
        }
        if self.network.connection_timeout == 0 {
            errors.push("network.connection_timeout must be greater than 0".to_string());
        }
        if self.network.connection_timeout > MAX_CONNECTION_TIMEOUT {
            errors.push(format!(
                "network.connection_timeout must be at most {MAX_CONNECTION_TIMEOUT} seconds"
            ));
        }
        // The liveness knob is one-way on purpose (see the field doc): a cadence *faster*
        // than the default is compatible with every peer, a cadence *slower* than it is
        // not, because a peer that enforces the default idle deadline (three default
        // cadences) drops a slower probe for silence. `0` is not "disabled" either: a
        // connection that is never probed is the half-open hole §6.20 closed.
        if self.network.keepalive_interval < MIN_KEEPALIVE_INTERVAL {
            errors.push(format!(
                "network.keepalive_interval must be at least {MIN_KEEPALIVE_INTERVAL} second, \
                 because a connection that is never probed cannot be detected as half-open"
            ));
        }
        if self.network.keepalive_interval > DEFAULT_KEEPALIVE_INTERVAL {
            errors.push(format!(
                "network.keepalive_interval must be at most {DEFAULT_KEEPALIVE_INTERVAL} seconds, \
                 the default: it may be shortened to survive a NAT that expires an idle mapping \
                 sooner, but a peer enforcing the default idle deadline would drop a slower probe"
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
        // No rotation is implemented, and none can be: the session key is the one every
        // peer holding the passphrase derives, and the identity key is what peers pin, so
        // a local timer would desynchronise the other end instead of rotating anything.
        // A non-zero value is therefore refused rather than accepted and ignored.
        if self.crypto.key_rotation_days != 0 {
            errors.push(
                "crypto.key_rotation_days must be 0 (no rotation): the session key is shared \
                 with every peer that has the passphrase and the identity key is what peers \
                 pin, so a rotation this end performed on a timer would leave them unable to \
                 read what it writes"
                    .to_string(),
            );
        }
        // The per-connection agreement is part of the identity handshake rather than a
        // switch: there is no downgrade path, so `false` cannot be honoured. Refusing it
        // beats accepting it and ignoring it, the rule `network.enable_upnp` follows.
        if !self.crypto.enable_pfs {
            errors.push(
                "crypto.enable_pfs cannot be false: the per-connection key agreement is part \
                 of the identity handshake and has no downgrade path; set it to true or \
                 remove the key"
                    .to_string(),
            );
        }

        // -- [ui] -----------------------------------------------------------
        // Only the themes the interface can draw are accepted: a name it cannot
        // honour used to be accepted and then ignored, which is the failure mode
        // `network.enable_upnp` is rejected for.
        if !SUPPORTED_THEMES
            .iter()
            .any(|name| name.eq_ignore_ascii_case(self.ui.theme.trim()))
        {
            errors.push(format!(
                "ui.theme must be one of {SUPPORTED_THEMES:?}, found '{}'",
                self.ui.theme
            ));
        }
        // `message_format` has no placeholder vocabulary yet, so a value other than
        // the shipped one would be silently dropped; the interface refuses it for
        // the same reason `enable_upnp = true` is refused. The default stays valid,
        // so a configuration written from the reference file still loads.
        if self.ui.message_format.trim() != DEFAULT_MESSAGE_FORMAT {
            errors.push(format!(
                "ui.message_format is not implemented (the interface renders message \
                 lines itself); set it to {DEFAULT_MESSAGE_FORMAT:?} or remove the key"
            ));
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

        // -- [ipc] ------------------------------------------------------------
        // `requests_per_second == 0` is valid on purpose: it disables the limiter,
        // which is the documented way to say "I trust every client on this
        // interface". A zero burst is not: it would mean "no request may ever be
        // served", which the rate already expresses as `0` and which an operator
        // can only have meant as a mistake.
        if self.ipc.request_burst == 0 {
            errors.push("ipc.request_burst must be greater than 0".to_string());
        }
        if self.ipc.requests_per_second > MAX_IPC_REQUESTS_PER_SECOND {
            errors.push(format!(
                "ipc.requests_per_second must be at most {MAX_IPC_REQUESTS_PER_SECOND}"
            ));
        }
        if self.ipc.request_burst > MAX_IPC_REQUEST_BURST {
            errors.push(format!(
                "ipc.request_burst must be at most {MAX_IPC_REQUEST_BURST}"
            ));
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
            // The shipped cadence is also the slowest accepted one; see the field doc.
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            // Not implemented, so the honest default is "off": the key cannot
            // promise a forwarded port the binary never asks for.
            enable_upnp: false,
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
            // No rotation is implemented, so the honest default is "none": the key could
            // not promise a rotation the binary never performs.
            key_rotation_days: 0,
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
            message_format: DEFAULT_MESSAGE_FORMAT.to_string(),
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

    /// A configuration file written before `[ipc]` existed still loads.
    ///
    /// Every other section is mandatory, so without `#[serde(default)]` on this one
    /// an operator's existing file would stop the process at startup — the section
    /// is policy, not a requirement.
    #[tokio::test]
    async fn test_a_configuration_without_an_ipc_section_still_loads() {
        let text = toml::to_string(&AppConfig::default()).expect("serialize the default");
        let start = text
            .find("[ipc]")
            .expect("the default serializes an [ipc] section");
        let older_file = &text[..start];

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        tokio::fs::write(&path, older_file).await.expect("write");

        let loaded = AppConfig::load(&path)
            .await
            .expect("a file without [ipc] must still load");
        assert_eq!(
            loaded.ipc.requests_per_second,
            crate::config::DEFAULT_REQUESTS_PER_SECOND
        );
        assert_eq!(
            loaded.ipc.request_burst,
            crate::config::DEFAULT_REQUEST_BURST
        );
        assert!(loaded.problems().is_empty(), "{:?}", loaded.problems());
    }

    /// Saving replaces the target atomically and leaves no temporary file.
    #[tokio::test]
    async fn test_config_save_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let temporary = dir.path().join("config.tmp");

        let config = AppConfig::default();
        config.save(&path).await.unwrap();

        assert!(path.exists(), "the target must exist after a save");
        assert!(
            !temporary.exists(),
            "the staging file must be renamed away, not left behind"
        );

        // Overwriting an existing file keeps it readable.
        config.save(&path).await.unwrap();
        assert!(AppConfig::load(&path).await.is_ok());
        assert!(!temporary.exists());
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

    /// The `[ui]` section only accepts settings the interface keeps.
    ///
    /// A theme it cannot draw and a message template it does not implement are
    /// refused, because accepting them would leave a user believing they took
    /// effect — the same reason `network.enable_upnp = true` is refused. The
    /// shipped values stay loadable, so the reference `config.toml` still works.
    #[test]
    fn test_ui_section_only_accepts_what_the_interface_honours() {
        let mut config = AppConfig::default();
        config.ui.theme = "solarized".to_string();
        let problems = config.problems();
        assert!(
            problems.iter().any(|p| p.contains("ui.theme")),
            "an unknown theme must be reported: {problems:?}"
        );

        config.ui.theme = "LIGHT".to_string();
        assert_eq!(
            config.problems(),
            Vec::<String>::new(),
            "the theme name is compared case-insensitively"
        );

        config.ui.message_format = "{sender} said {message}".to_string();
        let problems = config.problems();
        assert!(
            problems.iter().any(|p| p.contains("ui.message_format")),
            "a template the interface does not implement must be reported: {problems:?}"
        );

        // The reference value, and the remaining ui switches, stay valid.
        config.ui.message_format = DEFAULT_MESSAGE_FORMAT.to_string();
        config.ui.enable_colors = false;
        config.ui.enable_mouse = false;
        config.ui.auto_scroll = false;
        assert_eq!(config.problems(), Vec::<String>::new());
    }

    /// The `[crypto]` keys the binary cannot honour are refused, not ignored.
    ///
    /// `key_rotation_days` and `enable_pfs` had no reader outside this file: both were
    /// documented, deserialised and then dropped, which is the "accepted and ignored"
    /// failure `network.enable_upnp` is refused for. A rotation schedule cannot be
    /// honoured locally at all — the session key is the one every peer with the passphrase
    /// derives and the identity key is what peers pin, so a timer here would desynchronise
    /// the other end rather than rotate anything — and the per-connection agreement has no
    /// downgrade path, so each key keeps exactly one value.
    #[test]
    fn test_crypto_section_only_accepts_what_the_binary_honours() {
        let mut config = AppConfig::default();
        assert_eq!(
            config.crypto.key_rotation_days, 0,
            "the shipped default must be the only value the binary keeps"
        );
        assert!(config.problems().is_empty(), "{:?}", config.problems());

        config.crypto.key_rotation_days = 7;
        let problems = config.problems();
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("crypto.key_rotation_days")),
            "a rotation schedule must be reported: {problems:?}"
        );

        config.crypto.key_rotation_days = 0;
        config.crypto.enable_pfs = false;
        let problems = config.problems();
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("crypto.enable_pfs")),
            "disabling the key agreement must be reported: {problems:?}"
        );
        assert!(
            config.validate().is_err(),
            "a refused crypto key must stop the process"
        );

        // ... and the two keys the binary does keep stay loadable.
        config.crypto.enable_pfs = true;
        assert!(config.validate().is_ok(), "{:?}", config.problems());
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
            ("network.max_connections", |c| c.network.max_connections = 0),
            ("network.enable_upnp", |c| c.network.enable_upnp = true),
            ("network.connection_timeout (zero)", |c| {
                c.network.connection_timeout = 0;
            }),
            ("network.connection_timeout (upper)", |c| {
                c.network.connection_timeout = MAX_CONNECTION_TIMEOUT + 1;
            }),
            ("network.keepalive_interval (zero)", |c| {
                c.network.keepalive_interval = 0;
            }),
            (
                "network.keepalive_interval (slower than the default)",
                |c| {
                    c.network.keepalive_interval = DEFAULT_KEEPALIVE_INTERVAL + 1;
                },
            ),
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
                c.crypto.algorithm = "ROT13".to_string();
            }),
            ("crypto.kdf", |c| c.crypto.kdf = "md5".to_string()),
            ("crypto.key_rotation_days", |c| {
                c.crypto.key_rotation_days = 30;
            }),
            ("crypto.enable_pfs", |c| c.crypto.enable_pfs = false),
            ("ui.theme", |c| c.ui.theme = String::new()),
            ("ui.message_format", |c| c.ui.message_format = String::new()),
            ("logging.level", |c| c.logging.level = "verbose".to_string()),
            ("logging.file_path", |c| {
                c.logging.enable_file = true;
                c.logging.file_path = "  ".to_string();
            }),
            ("logging.rotation_size_mb", |c| {
                c.logging.rotation_size_mb = 0;
            }),
            ("logging.max_files", |c| c.logging.max_files = 0),
            ("ipc.request_burst", |c| c.ipc.request_burst = 0),
            ("ipc.requests_per_second (upper)", |c| {
                c.ipc.requests_per_second = MAX_IPC_REQUESTS_PER_SECOND + 1;
            }),
            ("ipc.request_burst (upper)", |c| {
                c.ipc.request_burst = MAX_IPC_REQUEST_BURST + 1;
            }),
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
        config.network.bootstrap_nodes = vec!["not-an-address".to_string()];
        config.crypto.kdf = "nope".to_string();

        let problems = config.problems();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("app.name")));
        assert!(problems.iter().any(|p| p.contains("bootstrap_nodes")));
        assert!(problems.iter().any(|p| p.contains("crypto.kdf")));
    }

    /// Port `0` is a supported configuration: the OS picks a free port.
    ///
    /// This is the case `README` describes for `/peers` ("useful when port `0`
    /// was configured"), the case every integration test starts from, and the
    /// value the Tox transport passes to toxcore when no port is pinned — so
    /// rejecting it in the two user-facing validators was a rule the documented
    /// behaviour could not satisfy.
    #[test]
    fn test_port_zero_means_the_os_picks_a_free_port() {
        let mut config = AppConfig::default();
        config.network.port = 0;
        assert!(config.problems().is_empty(), "{:?}", config.problems());
        assert!(config.validate().is_ok());
    }

    /// A configuration file written before `[network] keepalive_interval` existed still
    /// loads, with the cadence the transport used while it was a constant.
    ///
    /// This is what the field's `serde` default is for: the key was added after the
    /// shipped `config.toml` and after operators' files were written, and a missing key
    /// must not be the reason a working setup stops at startup. The value it defaults to
    /// is the one the constant had, so liveness does not change for those files.
    #[tokio::test]
    async fn test_a_configuration_without_a_keepalive_interval_still_loads() {
        let text = toml::to_string(&AppConfig::default()).expect("serialize the default");
        let older_file = text
            .lines()
            .filter(|line| !line.trim_start().starts_with("keepalive_interval"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !older_file.contains("keepalive_interval"),
            "the default must serialize the key, or this test proves nothing"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        tokio::fs::write(&path, older_file).await.expect("write");

        let loaded = AppConfig::load(&path)
            .await
            .expect("a file without keepalive_interval must still load");
        assert_eq!(
            loaded.network.keepalive_interval,
            DEFAULT_KEEPALIVE_INTERVAL
        );
        assert!(loaded.problems().is_empty(), "{:?}", loaded.problems());
    }

    /// The liveness cadence may be shortened, but neither slowed nor switched off.
    ///
    /// Shortening it is the whole point of the key — a NAT that expires an idle mapping
    /// sooner than the default cadence — and it is safe because probing faster only
    /// produces *more* evidence that this instance is alive. Slowing it is refused
    /// because the idle deadline is not derived from the value: a peer enforcing the
    /// default deadline would drop a slower probe, so allowing it would turn a tuning
    /// knob into a way of breaking connections that used to work. `0` is refused for a
    /// third reason: a connection that is never probed cannot be detected as half-open
    /// (`docs/ARCHITECTURE.md` §6.20).
    #[test]
    fn test_keepalive_interval_may_be_shortened_but_not_slowed() {
        let mut config = AppConfig::default();
        assert_eq!(
            config.network.keepalive_interval, DEFAULT_KEEPALIVE_INTERVAL,
            "the default must be the documented cadence"
        );

        // Every cadence from the floor up to the default is accepted.
        for seconds in MIN_KEEPALIVE_INTERVAL..=DEFAULT_KEEPALIVE_INTERVAL {
            config.network.keepalive_interval = seconds;
            assert!(
                config.problems().is_empty(),
                "{seconds}s must be accepted: {:?}",
                config.problems()
            );
        }

        for (seconds, why) in [
            (0_u64, "never probing cannot detect a half-open connection"),
            (
                DEFAULT_KEEPALIVE_INTERVAL + 1,
                "a peer's default deadline would drop this instance for silence",
            ),
        ] {
            config.network.keepalive_interval = seconds;
            let problems = config.problems();
            assert!(
                problems.iter().any(|p| p.contains("keepalive_interval")),
                "{seconds}s ({why}) must be rejected, got {problems:?}"
            );
            assert!(config.validate().is_err(), "{seconds}s must not validate");
        }
    }

    /// File logging is only required to have a path when it is enabled.
    #[test]
    fn test_logging_file_path_only_required_when_enabled() {
        let mut config = AppConfig::default();
        config.logging.enable_file = false;
        config.logging.file_path = String::new();
        config.logging.file_path = String::new();
        assert!(config.validate().is_ok());

        config.logging.enable_file = true;
        assert!(config.validate().is_err());
    }
}
