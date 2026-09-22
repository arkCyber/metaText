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
    ///
    /// `0` asks the operating system for a free port, which is what makes two
    /// instances on one machine startable without picking numbers by hand; the
    /// port actually bound is shown by `/whoami` and `/peers`.
    #[arg(
        short = 'p',
        long = "port",
        value_name = "PORT",
        global = true,
        help = "Network port for P2P communication (0 = let the OS pick a free one)"
    )]
    pub port: Option<u16>,

    /// Bootstrap node address
    #[arg(
        long = "bootstrap",
        value_name = "ADDRESS",
        global = true,
        help = "Bootstrap node: host:port for tcp, host:port:PUBLIC_KEY for tox"
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

    /// Number of messages `/history` shows when it is given no argument (`1`-`1000`).
    ///
    /// This used to be a knob with no reader: it was parsed, defaulted and validated,
    /// and then never reached the core, so `/history` always showed the protocol default
    /// while the flag claimed to keep `10000` messages. It now *is* the default the core
    /// serves a history request that names no limit with (a `/history <n>` argument still
    /// wins), which is why its range is the one the protocol can answer
    /// (`1..=MAX_HISTORY_LIMIT`) instead of a number nobody reads. Retention — deleting
    /// older messages — is a separate thing and is not implemented, so the flag no longer
    /// claims it.
    #[arg(
        long = "history-limit",
        value_name = "COUNT",
        default_value_t = meta_text_proto::ipc::protocol::DEFAULT_HISTORY_LIMIT,
        global = true,
        help = "Number of messages /history shows by default (1-1000)"
    )]
    pub history_limit: usize,

    /// Transport used to reach other users
    #[arg(
        long = "transport",
        value_enum,
        default_value_t = Transport::Tcp,
        global = true,
        help = "Transport to reach peers with: tcp (host:port) or tox (Tox address)"
    )]
    pub transport: Transport,

    /// Peer to connect to at startup
    #[arg(
        long = "peer",
        value_name = "ADDRESS",
        value_delimiter = ',',
        global = true,
        help = "Peer to reach at startup: host:port for tcp, a 76 character Tox address for tox; can be repeated"
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

    /// Sustained requests per second one client address may use (0 = no limit)
    ///
    /// Overrides `[ipc] requests_per_second` for one run. The budget belongs to the
    /// client's *address*, so it cannot be reset by reconnecting.
    #[arg(
        long = "ipc-rate",
        value_name = "COUNT",
        global = true,
        help = "Requests per second one client address may use (0 = no limit)"
    )]
    pub ipc_rate: Option<u32>,

    /// Burst one client address may spend above the sustained rate
    #[arg(
        long = "ipc-burst",
        value_name = "COUNT",
        global = true,
        help = "Burst one client address may spend above the sustained rate"
    )]
    pub ipc_burst: Option<u32>,

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

/// Number of hexadecimal characters in a Tox address (38 bytes × 2).
///
/// Re-exported from `meta-text-proto` (where the wire format is defined) rather
/// than defined here: it is a protocol constant, and the protocol crate is
/// compiled even when `tox` is behind the `tox-protocol` feature. A test in
/// `tox` asserts the two agree.
pub use meta_text_proto::TOX_ADDRESS_HEX_LEN;

/// Number of hexadecimal characters in a Tox public key (32 bytes × 2).
///
/// Used to validate a `--bootstrap host:port:PUBLIC_KEY` node and the `--peer`
/// variant that carries a key. Re-exported from `meta-text-proto` for the same
/// reason as [`TOX_ADDRESS_HEX_LEN`]; a test in `tox` asserts the two agree.
pub use meta_text_proto::PUBLIC_KEY_HEX_LEN;

/// Transport used to reach other users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Transport {
    /// The built-in TCP transport; peers are `host:port` addresses.
    #[default]
    Tcp,

    /// The Tox transport over `libtoxcore`; peers are 76 character Tox
    /// addresses. Requires a build with `--features tox-protocol` and a system
    /// `libtoxcore`.
    Tox,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(f, "tcp"),
            Self::Tox => write!(f, "tox"),
        }
    }
}

/// Application modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
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
    /// use meta_text_backend::cli::LogLevel;
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
    /// use meta_text_backend::cli::LogLevel;
    ///
    /// assert_eq!(LogLevel::Info.as_filter_str(), "info");
    /// assert_eq!(LogLevel::Trace.as_filter_str(), "trace");
    /// ```
    #[must_use]
    pub const fn as_filter_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
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
    /// use meta_text_backend::cli::{CliArgs, LogLevel};
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
    /// use meta_text_backend::cli::{CliArgs, LogLevel};
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
    /// use meta_text_backend::cli::{AppMode, CliArgs};
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
    /// use meta_text_backend::cli::CliArgs;
    ///
    /// let args = CliArgs::parse_from(&["meta-text"]);
    /// assert!(args.encryption_enabled());
    ///
    /// let args = CliArgs::parse_from(&["meta-text", "--no-encryption"]);
    /// assert!(!args.encryption_enabled());
    /// ```
    #[must_use]
    pub const fn encryption_enabled(&self) -> bool {
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
    /// use meta_text_backend::cli::CliArgs;
    ///
    /// let args = CliArgs::parse_from(&["meta-text"]);
    /// let data_dir = args.data_directory();
    /// assert!(data_dir.is_some());
    /// ```
    #[must_use]
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

    /// Check whether a string looks like a Tox address
    ///
    /// Only the shape is validated here (76 hexadecimal characters, i.e. the 38
    /// byte Tox address). toxcore additionally verifies the embedded checksum
    /// when the friend request is actually sent.
    ///
    /// # Returns
    ///
    /// Returns `true` when the value is exactly [`TOX_ADDRESS_HEX_LEN`]
    /// hexadecimal characters.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_backend::cli::{CliArgs, TOX_ADDRESS_HEX_LEN};
    ///
    /// assert!(CliArgs::is_valid_tox_address(&"A".repeat(TOX_ADDRESS_HEX_LEN)));
    /// assert!(!CliArgs::is_valid_tox_address("127.0.0.1:33445"));
    /// ```
    #[must_use]
    pub fn is_valid_tox_address(address: &str) -> bool {
        let trimmed = address.trim();
        trimmed.len() == TOX_ADDRESS_HEX_LEN
            && trimmed
                .chars()
                .all(|character| character.is_ascii_hexdigit())
    }

    /// Check whether a string looks like a Tox bootstrap node
    ///
    /// The accepted shape is `host:port:PUBLIC_KEY`, where the key is 32 bytes
    /// of hexadecimal (`64` characters). Bracketed IPv6 hosts are supported
    /// (`[::1]:33445:KEY`).
    ///
    /// Only the shape is validated here; the key is verified by toxcore when the
    /// node is actually dialled.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use meta_text_backend::cli::CliArgs;
    ///
    /// let key = "A".repeat(64);
    /// assert!(CliArgs::is_valid_tox_bootstrap(&format!("node.example.org:33445:{key}")));
    /// assert!(!CliArgs::is_valid_tox_bootstrap("node.example.org:33445"));
    /// assert!(!CliArgs::is_valid_tox_bootstrap("node.example.org:0:AAA"));
    /// ```
    #[must_use]
    pub fn is_valid_tox_bootstrap(value: &str) -> bool {
        let trimmed = value.trim();
        // Split the key off the right so a bracketed IPv6 host keeps its colons.
        let Some((host_port, key)) = trimmed.rsplit_once(':') else {
            return false;
        };
        let key_is_valid = key.len() == PUBLIC_KEY_HEX_LEN
            && key.chars().all(|character| character.is_ascii_hexdigit());

        key_is_valid && Self::is_valid_peer_address(host_port)
    }

    /// Validate command line arguments
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
    /// use meta_text_backend::cli::CliArgs;
    ///
    /// let args = CliArgs::parse_from(&["meta-text"]);
    /// assert!(args.validate().is_ok());
    /// ```
    ///
    /// # Errors
    ///
    /// Returns every problem found as a `String`: a port outside `u16` (refused
    /// by the parser itself), a `--peer` that is not a valid address for the
    /// selected transport, a bad `--bootstrap` node, an email that is not one, or
    /// a `--config` path that does not exist. The whole list is returned rather
    /// than the first, so a user can fix one invocation instead of one problem per
    /// run. An empty `Vec` is never an error.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        // Port `0` is deliberately *not* rejected: it means "let the OS pick a
        // free port" for both transports (`clap` already bounds the value to
        // `u16`). The bound port is reported by `/whoami` and `/peers`, so a
        // session that asked for `0` can still be dialled. Rejecting it here only
        // made the documented ephemeral-port case unreachable from the command
        // line while every integration test used it through `CliArgs`.

        // Validate max connections
        if self.max_connections == 0 {
            errors.push("Maximum connections must be greater than 0".to_string());
        }

        // The endpoint's policy flags are validated here for the same reason every
        // other number is: a value that cannot mean anything has to be refused
        // before a socket is bound, not discovered by a client later. A zero
        // *rate* is valid — it disables the limiter.
        if self.ipc_burst == Some(0) {
            errors.push("--ipc-burst must be greater than 0".to_string());
        }

        // Validate history limit. The value is the page the core serves a request that
        // names no limit with, so it has to be a page the core can answer: below one is
        // not a request, and above the protocol's bound would be clamped — a flag that
        // silently means something else than what was typed.
        if !(1..=meta_text_proto::ipc::protocol::MAX_HISTORY_LIMIT).contains(&self.history_limit) {
            errors.push(format!(
                "--history-limit must be between 1 and {}",
                meta_text_proto::ipc::protocol::MAX_HISTORY_LIMIT
            ));
        }

        // Validate peer addresses. The accepted shape depends on the transport:
        // the TCP transport dials `host:port`, the Tox transport needs a 76
        // character Tox address.
        for peer in &self.peers {
            let valid = match self.transport {
                Transport::Tcp => Self::is_valid_peer_address(peer),
                Transport::Tox => Self::is_valid_tox_address(peer),
            };
            if !valid {
                let expected = match self.transport {
                    Transport::Tcp => "host:port",
                    Transport::Tox => "a 76 character Tox address",
                };
                errors.push(format!(
                    "Invalid peer address for the {} transport (expected {expected}): {peer}",
                    self.transport
                ));
            }
        }

        // A bootstrap node is transport specific. TCP dials `host:port`; Tox
        // needs the node's public key as well, because `tox_bootstrap` cannot
        // work without it. Reporting a wrong shape beats dialling nothing.
        if let Some(node) = &self.bootstrap_node {
            match self.transport {
                Transport::Tcp => {
                    if !Self::is_valid_peer_address(node) {
                        errors.push(format!(
                            "Invalid bootstrap address (expected host:port): {node}"
                        ));
                    }
                }
                Transport::Tox => {
                    if !Self::is_valid_tox_bootstrap(node) {
                        errors.push(format!(
                            "--bootstrap for the tox transport expects \
                             host:port:PUBLIC_KEY, where PUBLIC_KEY is 64 hexadecimal \
                             characters: {node}"
                        ));
                    }
                }
            }
        }

        // Validate configuration file exists (if not default)
        if self.config_path.as_path() != std::path::Path::new("config.toml")
            && !self.config_path.exists()
        {
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
            transport: Transport::Tcp,
            log_level: None,
            headless: false,
            data_dir: None,
            port: None,
            bootstrap_node: None,
            debug: false,
            no_encryption: false,
            max_connections: 100,
            history_limit: meta_text_proto::ipc::protocol::DEFAULT_HISTORY_LIMIT,
            peers: Vec::new(),
            passphrase: None,
            nickname: None,
            ipc_listen: None,
            ipc_token: None,
            ipc_rate: None,
            ipc_burst: None,
            command: None,
        }
    }
}

impl std::fmt::Display for AppMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tui => write!(f, "TUI"),
            Self::Cli => write!(f, "CLI"),
            Self::Server => write!(f, "Server"),
            Self::Daemon => write!(f, "Daemon"),
            Self::Core => write!(f, "Core"),
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error => write!(f, "ERROR"),
            Self::Warn => write!(f, "WARN"),
            Self::Info => write!(f, "INFO"),
            Self::Debug => write!(f, "DEBUG"),
            Self::Trace => write!(f, "TRACE"),
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
        assert_eq!(
            args.history_limit,
            meta_text_proto::ipc::protocol::DEFAULT_HISTORY_LIMIT
        );
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

        // Port 0 means "the OS picks a free port", so it is valid.
        let ephemeral = CliArgs {
            port: Some(0),
            ..CliArgs::default()
        };
        assert!(
            ephemeral.validate().is_ok(),
            "{:?}",
            ephemeral.validate().unwrap_err()
        );

        // A value that is invalid regardless of the port is still reported.
        let invalid_args = CliArgs {
            max_connections: 0,
            ..CliArgs::default()
        };
        let result = invalid_args.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains(&"Maximum connections must be greater than 0".to_string()));

        // The endpoint's policy flags: a zero *rate* is the documented way to
        // disable the limit, a zero burst is not a policy at all.
        let unlimited =
            CliArgs::try_parse_from(["meta-text", "--ipc-rate", "0"]).expect("--ipc-rate 0 parses");
        assert_eq!(unlimited.ipc_rate, Some(0));
        assert!(unlimited.validate().is_ok());

        let no_burst = CliArgs::try_parse_from(["meta-text", "--ipc-burst", "0"])
            .expect("--ipc-burst 0 parses");
        assert!(no_burst
            .validate()
            .expect_err("a zero burst must be rejected")
            .iter()
            .any(|error| error.contains("--ipc-burst")));
    }

    /// `--history-limit` is bounded by what the core can actually serve.
    ///
    /// The flag is the page a `Request::History { limit: None }` is answered with, so a
    /// value the core would have to clamp (everything above
    /// `MAX_HISTORY_LIMIT`) or refuse (`0`) is reported before a socket is bound —
    /// otherwise the flag would silently mean a different number than the one typed.
    #[test]
    fn test_history_limit_is_bounded_by_the_protocol() {
        use meta_text_proto::ipc::protocol::{DEFAULT_HISTORY_LIMIT, MAX_HISTORY_LIMIT};

        let default = CliArgs::default();
        assert_eq!(default.history_limit, DEFAULT_HISTORY_LIMIT);
        assert!(default.validate().is_ok());

        let largest = CliArgs::try_parse_from([
            "meta-text",
            "--history-limit",
            &MAX_HISTORY_LIMIT.to_string(),
        ])
        .expect("the largest page parses");
        assert!(largest.validate().is_ok(), "{:?}", largest.validate());

        for refused in ["0", (MAX_HISTORY_LIMIT + 1).to_string().as_str(), "10000"] {
            let args = CliArgs::try_parse_from(["meta-text", "--history-limit", refused])
                .expect("a number parses");
            let errors = args
                .validate()
                .expect_err("an out of range page must be reported");
            assert!(
                errors.iter().any(|error| error.contains("--history-limit")),
                "{refused}: {errors:?}"
            );
        }
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

    /// The TCP transport is the default, so existing invocations are unchanged.
    #[test]
    fn test_transport_defaults_to_tcp() {
        let args = CliArgs::parse_from(["meta-text"]);
        assert_eq!(args.transport, Transport::Tcp);
        assert_eq!(CliArgs::default().transport, Transport::Tcp);
    }

    /// A `host:port` peer is accepted by tcp and rejected by tox, and vice versa.
    #[test]
    fn test_peer_shape_depends_on_the_transport() {
        let tox_address = "D5".to_string() + &"A".repeat(TOX_ADDRESS_HEX_LEN - 2);

        let tcp = CliArgs::parse_from(["meta-text", "--peer", "127.0.0.1:34567"]);
        assert_eq!(tcp.transport, Transport::Tcp);
        assert!(tcp.validate().is_ok(), "{:?}", tcp.validate());

        let tox = CliArgs::parse_from(["meta-text", "--transport", "tox", "--peer", &tox_address]);
        assert_eq!(tox.transport, Transport::Tox);
        assert!(tox.validate().is_ok(), "{:?}", tox.validate());

        // The shapes are not interchangeable.
        let wrong_for_tox = CliArgs::parse_from([
            "meta-text",
            "--transport",
            "tox",
            "--peer",
            "127.0.0.1:34567",
        ]);
        let errors = wrong_for_tox.validate().expect_err("must be rejected");
        assert!(errors.iter().any(|error| error.contains("Tox address")));

        let wrong_for_tcp =
            CliArgs::parse_from(["meta-text", "--transport", "tcp", "--peer", &tox_address]);
        let errors = wrong_for_tcp.validate().expect_err("must be rejected");
        assert!(errors.iter().any(|error| error.contains("host:port")));
    }

    /// `--bootstrap` has no meaning for the tox transport and is reported.
    #[test]
    fn test_bootstrap_is_rejected_for_tox() {
        let args = CliArgs::parse_from([
            "meta-text",
            "--transport",
            "tox",
            "--bootstrap",
            "node.example.org:33445",
        ]);
        let errors = args.validate().expect_err("must be rejected");
        assert!(
            errors.iter().any(|error| error.contains("--bootstrap")),
            "{errors:?}"
        );
    }

    /// Tox address recognition only checks the shape.
    #[test]
    fn test_tox_address_shape() {
        assert!(CliArgs::is_valid_tox_address(
            &"a".repeat(TOX_ADDRESS_HEX_LEN)
        ));
        assert!(CliArgs::is_valid_tox_address(
            "D5F0CFAD57CC86F558EC467F846566863855B8F85A27985BDE85B2F63F2FB24CB0222CC60831"
        ));
        assert!(!CliArgs::is_valid_tox_address(
            "A".repeat(TOX_ADDRESS_HEX_LEN - 1).as_str()
        ));
        assert!(!CliArgs::is_valid_tox_address("127.0.0.1:33445"));
        assert!(!CliArgs::is_valid_tox_address(
            &"Z".repeat(TOX_ADDRESS_HEX_LEN)
        ));
    }
}
