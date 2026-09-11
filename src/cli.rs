/*!
 * cli.rs
 *
 * Command line interface argument parsing for metaText messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Command line argument parsing with clap
 * - Configuration file path handling
 * - Verbosity level control
 * - Application mode selection
 */

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// Command line arguments for the metaText application
///
/// Defines all available command line options and their behavior.
/// Uses clap for automatic help generation and argument validation.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "meta-text",
    about = "metaText - A Web3 decentralized instant messaging client",
    version = env!("CARGO_PKG_VERSION"),
    author = env!("CARGO_PKG_AUTHORS"),
    long_about = "A secure, decentralized instant messaging client built with Rust, featuring end-to-end encryption and P2P communication."
)]
pub struct CliArgs {
    /// Configuration file path
    #[arg(
        short = 'c',
        long = "config",
        value_name = "FILE",
        default_value = "config.toml",
        global = true,
        help = "Path to configuration file"
    )]
    pub config_path: PathBuf,

    /// Application mode
    #[arg(
        short = 'm',
        long = "mode",
        value_enum,
        default_value_t = AppMode::Tui,
        global = true,
        help = "Application mode to run in"
    )]
    pub mode: AppMode,

    /// Log level explicitly requested on the command line
    ///
    /// Left unset by default so "no preference" can be told apart from an
    /// explicit request; an explicit value overrides `[logging] level` in the
    /// configuration file. Use [`CliArgs::effective_log_level`] for the
    /// resolved value.
    #[arg(
        short = 'l',
        long = "log-level",
        value_enum,
        global = true,
        help = "Set the logging level (overrides [logging] level)"
    )]
    pub log_level: Option<LogLevel>,

    /// Run in headless mode (no TUI)
    #[arg(
        long = "headless",
        global = true,
        help = "Run in headless mode without user interface"
    )]
    pub headless: bool,

    /// Data directory path
    #[arg(
        short = 'd',
        long = "data-dir",
        value_name = "DIR",
        global = true,
        help = "Directory to store application data"
    )]
    pub data_dir: Option<PathBuf>,

    /// Network port for P2P communication
    #[arg(
        short = 'p',
        long = "port",
        value_name = "PORT",
        global = true,
        help = "Network port for P2P communication"
    )]
    pub port: Option<u16>,

    /// Bootstrap node address
    #[arg(
        long = "bootstrap",
        value_name = "ADDRESS",
        global = true,
        help = "Bootstrap node address for network discovery"
    )]
    pub bootstrap_node: Option<String>,

    /// Enable debug mode
    #[arg(
        short = 'D',
        long = "debug",
        global = true,
        help = "Enable debug mode with additional logging"
    )]
    pub debug: bool,

    /// Disable encryption
    #[arg(
        long = "no-encryption",
        global = true,
        help = "Disable message encryption (not recommended)"
    )]
    pub no_encryption: bool,

    /// Maximum number of connections
    #[arg(
        long = "max-connections",
        value_name = "COUNT",
        default_value = "100",
        global = true,
        help = "Maximum number of concurrent connections"
    )]
    pub max_connections: u32,

    /// Message history limit
    #[arg(
        long = "history-limit",
        value_name = "COUNT",
        default_value = "10000",
        global = true,
        help = "Maximum number of messages to keep in history"
    )]
    pub history_limit: usize,

    /// Peer to connect to at startup
    #[arg(
        long = "peer",
        value_name = "ADDRESS",
        value_delimiter = ',',
        global = true,
        help = "Peer address (host:port) to connect to; can be repeated"
    )]
    pub peers: Vec<String>,

    /// Shared passphrase used to derive the session encryption key
    #[arg(
        long = "passphrase",
        value_name = "TEXT",
        env = "METATEXT_PASSPHRASE",
        global = true,
        help = "Shared passphrase; peers using the same value can decrypt each other"
    )]
    pub passphrase: Option<String>,

    /// Nickname to announce to peers
    #[arg(
        long = "nick",
        value_name = "NAME",
        global = true,
        help = "Nickname announced to peers on connection"
    )]
    pub nickname: Option<String>,

    /// Address to expose the core protocol on (headless modes)
    #[arg(
        long = "ipc-listen",
        value_name = "ADDRESS",
        global = true,
        help = "Host:port to serve the core protocol on; omit to stay private"
    )]
    pub ipc_listen: Option<String>,

    /// Shared secret required by protocol clients
    #[arg(
        long = "ipc-token",
        value_name = "TOKEN",
        env = "METATEXT_IPC_TOKEN",
        global = true,
        help = "Token a protocol client must present on --ipc-listen"
    )]
    pub ipc_token: Option<String>,

    /// Optional top level subcommand
    ///
    /// When omitted the [`CliArgs::mode`] flag selects the interface. `run` is
    /// a convenience alias that selects the interactive REPL
    /// ([`AppMode::Cli`]); see [`CliArgs::effective_mode`].
    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

/// Top level subcommands accepted by the `meta-text` binary
///
/// Every option on [`CliArgs`] is global, so it is also accepted *after* the
/// subcommand, e.g. `meta-text run --port 34567`.
#[derive(Subcommand, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliCommand {
    /// Start the interactive REPL (same as `--mode cli`)
    Run,
}

/// Application modes
#[derive(Debug, Clone, Copy, PartialEq, ValueEnum)]
pub enum AppMode {
    /// Terminal user interface mode
    Tui,

    /// Command line interface mode
    Cli,

    /// Server mode for background operation
    Server,

    /// Daemon mode for system service
    Daemon,

    /// Headless core service only (no user interface attached)
    Core,
}

/// Logging levels
#[derive(Debug, Clone, Copy, PartialEq, ValueEnum)]
pub enum LogLevel {
    /// Error level only
    Error,

    /// Warning level and above
    Warn,

    /// Info level and above
    Info,

    /// Debug level and above
    Debug,

    /// Trace level and above
    Trace,
}

impl LogLevel {
    /// Parse a log level from its lower case name.
    ///
    /// # Arguments
    ///
    /// * `name` - Candidate level name, matched case-insensitively.
    ///
    /// # Returns
    ///
    /// Returns the matching level, or `None` for an unknown name.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::cli::LogLevel;
    ///
    /// assert_eq!(LogLevel::parse("debug"), Some(LogLevel::Debug));
    /// assert_eq!(LogLevel::parse("DEBUG"), Some(LogLevel::Debug));
    /// assert_eq!(LogLevel::parse("verbose"), None);
    /// ```
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    /// Get the stable filter directive for this log level
    ///
    /// The returned value is lower case and can be fed directly to
    /// `tracing_subscriber::EnvFilter` as a directive.
    ///
    /// # Returns
    ///
    /// Returns the lower case name of the level, e.g. `"info"`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text::cli::LogLevel;
    ///
    /// assert_eq!(LogLevel::Info.as_filter_str(), "info");
    /// assert_eq!(LogLevel::Trace.as_filter_str(), "trace");
    /// ```
    #[must_use]
    pub const fn as_filter_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

impl CliArgs {
    /// Get the effective log level considering debug flag
    ///
    /// The returned value is the level to fall back to when neither `RUST_LOG`
    /// nor an explicit `--log-level` is available; it defaults to
    /// [`LogLevel::Info`] and becomes [`LogLevel::Debug`] when `--debug` is set.
    ///
    /// # Returns
    ///
    /// Returns the effective log level, which will be Debug if the debug flag
    /// is set, regardless of the explicit log level setting.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use clap::Parser;
    /// use meta_text::cli::{CliArgs, LogLevel};
    ///
    /// let args = CliArgs::parse_from(["meta-text", "--debug"]);
    /// assert_eq!(args.effective_log_level(), LogLevel::Debug);
    ///
    /// let args = CliArgs::parse_from(["meta-text"]);
    /// assert_eq!(args.effective_log_level(), LogLevel::Info);
    /// ```
    #[must_use]
    pub fn effective_log_level(&self) -> LogLevel {
        self.log_level_override().unwrap_or(LogLevel::Info)
    }

    /// The log level the user asked for on the command line, if any
    ///
    /// This is what lets an explicit `--log-level` (or `--debug`) override the
    /// `[logging] level` value in the configuration file instead of being
    /// shadowed by it. `--debug` is treated as an explicit request for
    /// [`LogLevel::Debug`] and wins over `--log-level`.
    ///
    /// # Returns
    ///
    /// Returns `Some(level)` when the command line expressed a preference, or
    /// `None` when the configuration file should decide.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use clap::Parser;
    /// use meta_text::cli::{CliArgs, LogLevel};
    ///
    /// let args = CliArgs::parse_from(["meta-text"]);
    /// assert_eq!(args.log_level_override(), None);
    ///
    /// let args = CliArgs::parse_from(["meta-text", "--log-level", "warn"]);
    /// assert_eq!(args.log_level_override(), Some(LogLevel::Warn));
    /// ```
    #[must_use]
    pub const fn log_level_override(&self) -> Option<LogLevel> {
        if self.debug {
            Some(LogLevel::Debug)
        } else {
            self.log_level
        }
    }

    /// Resolve the interface mode, letting an explicit subcommand win
    ///
    /// The `run` subcommand is a convenience alias for the interactive REPL:
    /// when it is present the resolved mode is [`AppMode::Cli`], otherwise the
    /// value of `--mode` is used unchanged.
    ///
    /// # Returns
    ///
    /// Returns the [`AppMode`] the application should start in.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use clap::Parser;
    /// use meta_text::cli::{AppMode, CliArgs};
    ///
    /// let args = CliArgs::parse_from(["meta-text", "run"]);
    /// assert_eq!(args.effective_mode(), AppMode::Cli);
    ///
    /// let args = CliArgs::parse_from(["meta-text", "--mode", "tui"]);
    /// assert_eq!(args.effective_mode(), AppMode::Tui);
    /// ```
    #[must_use]
    pub const fn effective_mode(&self) -> AppMode {
        match self.command {
            Some(CliCommand::Run) => AppMode::Cli,
            None => self.mode,
        }
    }

    /// Check if encryption is enabled
    ///
    /// # Returns
    ///
    /// Returns `true` if encryption is enabled, `false` if disabled.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use clap::Parser;
    /// use meta_text::cli::CliArgs;
    ///
    /// let args = CliArgs::parse_from(&["meta-text"]);
    /// assert!(args.encryption_enabled());
    ///
    /// let args = CliArgs::parse_from(&["meta-text", "--no-encryption"]);
    /// assert!(!args.encryption_enabled());
    /// ```
    pub fn encryption_enabled(&self) -> bool {
        !self.no_encryption
    }

    /// Get the default data directory if not specified
    ///
    /// # Returns
    ///
    /// Returns the data directory path, using the user's home directory
    /// if not explicitly specified.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use clap::Parser;
    /// use meta_text::cli::CliArgs;
    ///
    /// let args = CliArgs::parse_from(&["meta-text"]);
    /// let data_dir = args.data_directory();
    /// assert!(data_dir.is_some());
    /// ```
    pub fn data_directory(&self) -> Option<PathBuf> {
        self.data_dir.clone().or_else(|| {
            dirs::data_dir().map(|mut path| {
                path.push("meta-text");
                path
            })
        })
    }

    /// Check whether a `host:port` string is a plausible peer address
    ///
    /// Host names are not resolved here; only the shape is validated (non-empty
    /// host and a non-zero port) so that a typo is reported before connecting.
    ///
    /// # Arguments
    ///
    /// * `address` - Candidate `host:port` string
    ///
    /// # Returns
    ///
    /// Returns `true` when the address has a non-empty host and a valid port.
    #[must_use]
    pub fn is_valid_peer_address(address: &str) -> bool {
        crate::utils::is_valid_host_port(address)
    }

    /// Validate command line arguments
    ///
    /// Performs validation checks on the provided arguments and returns
    /// any validation errors found.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if all arguments are valid, or a list of validation
    /// errors if any issues are found.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use clap::Parser;
    /// use meta_text::cli::CliArgs;
    ///
    /// let args = CliArgs::parse_from(&["meta-text"]);
    /// assert!(args.validate().is_ok());
    /// ```
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        // Validate port range
        if let Some(port) = self.port {
            if port == 0 {
                errors.push("Port cannot be 0".to_string());
            }
        }

        // Validate max connections
        if self.max_connections == 0 {
            errors.push("Maximum connections must be greater than 0".to_string());
        }

        // Validate history limit
        if self.history_limit == 0 {
            errors.push("History limit must be greater than 0".to_string());
        }

        // Validate peer addresses (expected `host:port` with a non-zero port)
        for peer in &self.peers {
            if !Self::is_valid_peer_address(peer) {
                errors.push(format!("Invalid peer address (expected host:port): {peer}"));
            }
        }

        // Validate the optional bootstrap node with the same rule
        if let Some(node) = &self.bootstrap_node {
            if !Self::is_valid_peer_address(node) {
                errors.push(format!(
                    "Invalid bootstrap address (expected host:port): {node}"
                ));
            }
        }

        // Validate configuration file exists (if not default)
        if self.config_path != PathBuf::from("config.toml") && !self.config_path.exists() {
            errors.push(format!(
                "Configuration file does not exist: {}",
                self.config_path.display()
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            config_path: PathBuf::from("config.toml"),
            mode: AppMode::Tui,
            log_level: None,
            headless: false,
            data_dir: None,
            port: None,
            bootstrap_node: None,
            debug: false,
            no_encryption: false,
            max_connections: 100,
            history_limit: 10000,
            peers: Vec::new(),
            passphrase: None,
            nickname: None,
            ipc_listen: None,
            ipc_token: None,
            command: None,
        }
    }
}

impl std::fmt::Display for AppMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppMode::Tui => write!(f, "TUI"),
            AppMode::Cli => write!(f, "CLI"),
            AppMode::Server => write!(f, "Server"),
            AppMode::Daemon => write!(f, "Daemon"),
            AppMode::Core => write!(f, "Core"),
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Error => write!(f, "ERROR"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Trace => write!(f, "TRACE"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_args_default() {
        let args = CliArgs::default();

        assert_eq!(args.config_path, PathBuf::from("config.toml"));
        assert_eq!(args.mode, AppMode::Tui);
        assert_eq!(args.log_level, None);
        assert!(!args.headless);
        assert!(!args.debug);
        assert!(args.encryption_enabled());
        assert_eq!(args.max_connections, 100);
        assert_eq!(args.history_limit, 10000);
        assert!(args.command.is_none());
    }

    #[test]
    fn test_effective_log_level() {
        let mut args = CliArgs::default();

        // Without debug flag
        assert_eq!(args.effective_log_level(), LogLevel::Info);

        // With debug flag
        args.debug = true;
        assert_eq!(args.effective_log_level(), LogLevel::Debug);
    }

    /// An explicit `--log-level` is reported as an override; `--debug` wins.
    #[test]
    fn test_log_level_override() {
        assert_eq!(CliArgs::default().log_level_override(), None);

        let args = CliArgs::parse_from(["meta-text", "--log-level", "warn"]);
        assert_eq!(args.log_level_override(), Some(LogLevel::Warn));
        assert_eq!(args.effective_log_level(), LogLevel::Warn);

        // `--debug` is the stronger request even next to `--log-level`.
        let args = CliArgs::parse_from(["meta-text", "--log-level", "error", "--debug"]);
        assert_eq!(args.log_level_override(), Some(LogLevel::Debug));
    }

    #[test]
    fn test_encryption_enabled() {
        let mut args = CliArgs::default();

        // Default should be enabled
        assert!(args.encryption_enabled());

        // With no-encryption flag
        args.no_encryption = true;
        assert!(!args.encryption_enabled());
    }

    #[test]
    fn test_validation() {
        let args = CliArgs::default();
        assert!(args.validate().is_ok());

        // Test invalid port
        let mut invalid_args = CliArgs::default();
        invalid_args.port = Some(0);
        let result = invalid_args.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains(&"Port cannot be 0".to_string()));
    }

    #[test]
    fn test_app_mode_display() {
        assert_eq!(AppMode::Tui.to_string(), "TUI");
        assert_eq!(AppMode::Cli.to_string(), "CLI");
        assert_eq!(AppMode::Server.to_string(), "Server");
        assert_eq!(AppMode::Daemon.to_string(), "Daemon");
    }

    #[test]
    fn test_log_level_display() {
        assert_eq!(LogLevel::Error.to_string(), "ERROR");
        assert_eq!(LogLevel::Warn.to_string(), "WARN");
        assert_eq!(LogLevel::Info.to_string(), "INFO");
        assert_eq!(LogLevel::Debug.to_string(), "DEBUG");
        assert_eq!(LogLevel::Trace.to_string(), "TRACE");
    }

    #[test]
    fn test_log_level_filter_str() {
        assert_eq!(LogLevel::Error.as_filter_str(), "error");
        assert_eq!(LogLevel::Warn.as_filter_str(), "warn");
        assert_eq!(LogLevel::Info.as_filter_str(), "info");
        assert_eq!(LogLevel::Debug.as_filter_str(), "debug");
        assert_eq!(LogLevel::Trace.as_filter_str(), "trace");
    }

    /// `run` is parsed as a subcommand and selects the interactive REPL
    #[test]
    fn test_run_subcommand_selects_cli_mode() {
        let args = CliArgs::parse_from(["meta-text", "run"]);

        assert_eq!(args.command, Some(CliCommand::Run));
        assert_eq!(args.effective_mode(), AppMode::Cli);
    }

    /// Global options are accepted after the subcommand as well
    #[test]
    fn test_global_options_after_subcommand() {
        let args = CliArgs::parse_from([
            "meta-text",
            "run",
            "--port",
            "34567",
            "--nick",
            "Alice",
            "--peer",
            "127.0.0.1:34568",
        ]);

        assert_eq!(args.effective_mode(), AppMode::Cli);
        assert_eq!(args.port, Some(34567));
        assert_eq!(args.nickname.as_deref(), Some("Alice"));
        assert_eq!(args.peers, vec!["127.0.0.1:34568".to_string()]);
        // Unrelated defaults are preserved through the subcommand.
        assert_eq!(args.log_level, None);
        assert_eq!(args.effective_log_level(), LogLevel::Info);
        assert_eq!(args.command, Some(CliCommand::Run));

        // A log level given *after* `run` is still parsed as a global option.
        let args = CliArgs::parse_from(["meta-text", "run", "--log-level", "warn"]);
        assert_eq!(args.log_level, Some(LogLevel::Warn));
        assert_eq!(args.effective_log_level(), LogLevel::Warn);
    }

    /// Without a subcommand `--mode` keeps deciding the interface
    #[test]
    fn test_mode_flag_still_works_without_subcommand() {
        let args = CliArgs::parse_from(["meta-text", "--mode", "cli"]);

        assert!(args.command.is_none());
        assert_eq!(args.effective_mode(), AppMode::Cli);
    }

    /// An explicit `run` wins over a conflicting `--mode`
    #[test]
    fn test_run_subcommand_overrides_mode_flag() {
        let args = CliArgs::parse_from(["meta-text", "--mode", "tui", "run"]);

        assert_eq!(args.command, Some(CliCommand::Run));
        assert_eq!(args.effective_mode(), AppMode::Cli);
    }
}
