/*!
 * main.rs
 *
 * Main entry point for metaText - A Web3 decentralized instant messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - End-to-end encrypted messaging
 * - Decentralized P2P communication
 * - Cross-platform TUI interface
 * - Group chat support
 * - File transfer capabilities
 * - Web3 integration
 */

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    clippy::cargo,
    rust_2018_idioms
)]

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use meta_text::cli::{AppMode, CliArgs, LogLevel};
use meta_text::config::AppConfig;
use meta_text::MetaTextApp;

/// Main entry point for the metaText application
///
/// Initializes logging, parses command line arguments, loads configuration,
/// and starts the appropriate application mode (TUI or CLI).
///
/// # Returns
///
/// Returns `Ok(())` on successful execution, or an error if initialization
/// or execution fails.
///
/// # Errors
///
/// This function will return an error if:
/// - Logging initialization fails
/// - Configuration loading fails
/// - Application initialization fails
/// - Application execution encounters an unrecoverable error
///
/// # Examples
///
/// ```rust
/// use meta_text::MetaTextApp;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     // Application will be initialized and run here
///     Ok(())
/// }
/// ```
#[tokio::main]
async fn main() -> Result<()> {
    let timestamp = chrono::Utc::now();

    // Parse command line arguments first so that the requested verbosity can be
    // applied to the logging system before anything else runs.
    let args = CliArgs::parse();

    // Initialize logging system.
    // The returned guard MUST stay alive for the whole program, otherwise the
    // non-blocking log writer is dropped and file logs are silently lost.
    // When the full screen TUI takes over the terminal the console layer is
    // disabled so log lines cannot corrupt the rendered interface.
    let console_logging = !(cfg!(feature = "terminal-ui")
        && matches!(args.effective_mode(), AppMode::Tui)
        && !args.headless);
    let _log_guard = init_logging(args.effective_log_level(), console_logging)
        .context("Failed to initialize logging system")?;

    info!(
        "🚀 [{}] Starting metaText v{}",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        env!("CARGO_PKG_VERSION")
    );
    info!(
        "📧 [{}] Author: {}",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        env!("CARGO_PKG_AUTHORS")
    );
    info!(
        "📝 [{}] Description: {}",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        env!("CARGO_PKG_DESCRIPTION")
    );
    info!(
        "⚙️ [{}] Parsed command line arguments: {:?}",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        args
    );

    // Validate the parsed arguments before doing any expensive work
    if let Err(errors) = args.validate() {
        for message in &errors {
            error!(
                "❌ [{}] Invalid argument: {}",
                timestamp.format("%Y-%m-%d %H:%M:%S"),
                message
            );
        }
        bail!("Invalid command line arguments: {}", errors.join("; "));
    }

    // Load application configuration
    let config = AppConfig::load(&args.config_path)
        .await
        .context("Failed to load application configuration")?;
    info!(
        "📋 [{}] Configuration loaded successfully from: {:?}",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        args.config_path
    );

    // Create and initialize the application
    let mut app = MetaTextApp::new(config, args)
        .await
        .context("Failed to create MetaText application")?;
    info!(
        "✅ [{}] MetaText application initialized successfully",
        timestamp.format("%Y-%m-%d %H:%M:%S")
    );

    // Run the application
    match app.run().await {
        Ok(()) => {
            info!(
                "👋 [{}] MetaText application shutdown gracefully",
                timestamp.format("%Y-%m-%d %H:%M:%S")
            );
            Ok(())
        }
        Err(e) => {
            error!(
                "❌ [{}] MetaText application error: {:?}",
                timestamp.format("%Y-%m-%d %H:%M:%S"),
                e
            );
            Err(e)
        }
    }
}

/// Initialize the logging system with structured logging support
///
/// Sets up tracing with layered output: a daily rotating file under `logs/` and
/// an ANSI coloured console layer on stdout.
///
/// The log level is taken from the `RUST_LOG` environment variable when it is
/// set; otherwise the `default_level` argument is used.
///
/// # Arguments
///
/// * `default_level` - Verbosity to apply when `RUST_LOG` is not set. Callers
///   usually derive this from `--log-level` / `--debug` via
///   [`meta_text::cli::CliArgs::effective_log_level`].
/// * `console` - Whether to also mirror log records to `stdout`. This must be
///   `false` while the full screen TUI owns the terminal, because any write to
///   `stdout` would corrupt the rendered interface.
///
/// # Returns
///
/// Returns the [`tracing_appender::non_blocking::WorkerGuard`] that keeps the
/// non-blocking file writer alive. The caller must keep this guard in scope for
/// as long as logging is required; dropping it flushes and stops file logging.
///
/// # Errors
///
/// This function will return an error if:
/// - The tracing subscriber cannot be initialized
/// - File logging setup fails
///
/// # Examples
///
/// ```rust,no_run
/// use meta_text::cli::LogLevel;
///
/// # fn init_logging(_level: LogLevel, _console: bool) -> anyhow::Result<()> { Ok(()) }
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     init_logging(LogLevel::Info, true)?;
///     tracing::info!("Logging system is ready");
///     Ok(())
/// }
/// ```
fn init_logging(
    default_level: LogLevel,
    console: bool,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    // File appender for logging
    let file_appender = tracing_appender::rolling::daily("logs", "meta-text.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // `RUST_LOG` takes precedence; fall back to the requested CLI level.
    let build_filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(default_level.as_filter_str()))
    };

    // Always log to the rotating file; mirror to the console only when asked.
    if console {
        tracing_subscriber::registry()
            .with(build_filter())
            .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
            .with(fmt::layer().with_writer(std::io::stdout).with_ansi(true))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(build_filter())
            .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
            .init();
    }

    info!(
        "📊 [{}] Logging system initialized successfully (console={})",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
        console
    );
    info!(
        "📁 [{}] Log files will be written to: ./logs/meta-text.log.YYYY-MM-DD",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
    );

    // Hand the guard back to the caller so that it lives for the whole process.
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the logging system initializes without errors
    #[tokio::test]
    async fn test_logging_initialization() -> Result<()> {
        // Note: In real tests, we might want to use a test-specific logging setup
        // to avoid interfering with other tests
        let _guard =
            init_logging(LogLevel::Info, false).context("Logging initialization should succeed")?;

        // Test that we can write log messages after initialization
        info!(
            "🧪 [{}] Test log message from logging initialization test",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
        );
        warn!(
            "⚠️ [{}] Test warning message",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
        );
        error!(
            "❌ [{}] Test error message (this is expected in tests)",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
        );

        Ok(())
    }

    /// The startup validation path rejects invalid command line arguments
    #[tokio::test]
    async fn test_main_argument_validation() {
        // A valid invocation parses and validates cleanly.
        let valid = CliArgs::try_parse_from(["meta-text", "--mode", "cli"]).expect("valid args");
        assert!(valid.validate().is_ok());

        // Port 0 is rejected by validation.
        let invalid = CliArgs::try_parse_from(["meta-text", "--port", "0"]).expect("parses");
        let errors = invalid.validate().expect_err("port 0 must be rejected");
        assert!(errors
            .iter()
            .any(|error| error.contains("Port cannot be 0")));

        // An unknown mode cannot be parsed at all.
        assert!(CliArgs::try_parse_from(["meta-text", "--mode", "nope"]).is_err());
    }

    /// The `run` subcommand resolves to the interactive REPL and validates
    #[tokio::test]
    async fn test_run_subcommand_arguments() {
        let args = CliArgs::try_parse_from(["meta-text", "run", "--port", "34567"])
            .expect("valid run invocation");

        assert_eq!(args.effective_mode(), AppMode::Cli);
        assert!(args.validate().is_ok());
    }

    /// A missing configuration file is created with defaults on first start
    #[tokio::test]
    async fn test_main_creates_default_configuration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        let config = AppConfig::load(&path)
            .await
            .expect("missing config should be created");
        assert!(path.exists());
        assert_eq!(config.app.name, "metaText");
        assert_eq!(config.app.version, env!("CARGO_PKG_VERSION"));
    }
}
