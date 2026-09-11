/*!
 * main.rs
 *
 * Process entry point for metaText: starts the core service and attaches the
 * requested user interface.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - End-to-end encrypted messaging
 * - Decentralized P2P communication
 * - CLI and TUI front-ends, both driven by the core protocol
 * - Optional headless core service that exposes the protocol over TCP
 *
 * # Layering
 *
 * ```text
 *   main (composition root)
 *     |- CoreService::spawn()          backend task
 *     |- CliFrontend / TuiFrontend     presentation task
 *     `- CoreServer (optional)         out-of-process front-ends
 * ```
 *
 * The front-ends hold no domain state and cannot reach `crypto`, `database`
 * or `network`; they only speak the protocol in `meta_text::ipc`.
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
use meta_text::ipc::client::LocalClient;
use meta_text::ipc::server::{CoreServer, ServerOptions};
use meta_text::ipc::{CoreHandle, CoreService};
use meta_text::ui::{CliFrontend, TuiFrontend};

/// Main entry point for the metaText application.
///
/// Initializes logging, parses and validates command line arguments, starts
/// the core service and attaches the requested front-end.
///
/// # Returns
///
/// Returns `Ok(())` on a clean shutdown, or an error when initialisation or
/// execution fails.
///
/// # Errors
///
/// This function will return an error if:
/// - Logging initialization fails
/// - Command line arguments are invalid
/// - Configuration loading fails
/// - The core service fails to start or stop
/// - The requested front-end fails to run
#[tokio::main]
async fn main() -> Result<()> {
    let timestamp = chrono::Utc::now();

    // 1. Command line first: it is the cheapest input and it may point at a
    //    different configuration file.
    let args = CliArgs::parse();
    let mode = args.effective_mode();

    if let Err(errors) = args.validate() {
        // Not logged yet on purpose: logging may need the configuration that
        // this very argument list selects.
        bail!("Invalid command line arguments: {}", errors.join("; "));
    }

    // 2. Configuration next, so the logging system can honour it and an
    //    unusable configuration stops the process before any socket is bound.
    let config = AppConfig::load(&args.config_path)
        .await
        .context("Failed to load application configuration")?;
    if let Err(error) = config.validate() {
        bail!("{error}");
    }
    let app_name = config.app.name.clone();

    // 3. Logging: the returned guard MUST stay alive for the whole program,
    //    otherwise the non-blocking log writer is dropped and file logs are
    //    silently lost. When the full screen TUI takes over the terminal the
    //    console layer is disabled so log lines cannot corrupt the interface.
    let console_logging =
        !(cfg!(feature = "terminal-ui") && matches!(mode, AppMode::Tui) && !args.headless);
    let _log_guard = init_logging(&config.logging, args.log_level_override(), console_logging)
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
        "⚙️ [{}] Parsed command line arguments: {:?}",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        args
    );
    info!(
        "📋 [{}] Configuration loaded and validated from: {:?}",
        timestamp.format("%Y-%m-%d %H:%M:%S"),
        args.config_path
    );

    // Build and start the backend, then detach it onto its own task.
    let mut service = CoreService::new(config, args.clone())
        .await
        .context("Failed to create the core service")?;
    service
        .start()
        .await
        .context("Failed to start the core service")?;
    let core = service.spawn();
    info!(
        "✅ [{}] Core service running; attaching the '{mode}' interface",
        timestamp.format("%Y-%m-%d %H:%M:%S")
    );

    match run_interface(&core, &args, mode, &app_name).await {
        Ok(()) => {
            info!(
                "👋 [{}] MetaText application shutdown gracefully",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
            );
            Ok(())
        }
        Err(error) => {
            error!(
                "❌ [{}] MetaText application error: {:?}",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
                error
            );
            Err(error)
        }
    }
}

/// Attach the interface selected by the command line.
async fn run_interface(
    core: &CoreHandle,
    args: &CliArgs,
    mode: AppMode,
    app_name: &str,
) -> Result<()> {
    let client = LocalClient::new(core.clone());

    match mode {
        // Headless modes never touch the terminal.
        AppMode::Server | AppMode::Daemon | AppMode::Core => run_headless(core, args).await,
        AppMode::Tui if !args.headless => {
            let mut frontend = TuiFrontend::start(client, app_name, true)
                .await
                .context("Failed to start the terminal interface")?;
            frontend.run().await
        }
        AppMode::Cli if !args.headless => {
            let mut frontend = CliFrontend::new(client);
            frontend.run().await
        }
        // `--headless` (and every remaining combination) hosts the backend.
        _ => run_headless(core, args).await,
    }
}

/// Host the backend without a user interface.
///
/// When `--ipc-listen` is given the core protocol is exposed on that address so
/// other front-ends can attach; otherwise the process simply idles until it
/// receives SIGINT or SIGTERM.
async fn run_headless(core: &CoreHandle, args: &CliArgs) -> Result<()> {
    let server = match args.ipc_listen.as_deref() {
        Some(address) => {
            let endpoint = CoreServer::bind(address)
                .await
                .with_context(|| format!("Failed to bind the core protocol to {address}"))?;
            if let Ok(local) = endpoint.local_addr() {
                info!("🛰️ Core protocol endpoint bound to {local}");
            }
            let options = ServerOptions {
                token: args.ipc_token.clone(),
                name: "meta-text".to_string(),
            };
            if options.token.is_none() {
                warn!(
                    "⚠️ --ipc-listen without --ipc-token: any local process can attach. \
                     Prefer a loopback address or set a token."
                );
            }
            let owned = core.clone();
            Some(tokio::spawn(
                async move { endpoint.run(owned, options).await },
            ))
        }
        None => None,
    };

    wait_for_termination().await;
    info!("🛑 Shutdown signal received");

    if let Some(task) = server {
        task.abort();
    }
    if let Err(error) = core.shutdown().await {
        warn!("⚠️ The core service did not acknowledge shutdown: {error}");
    }
    Ok(())
}

/// Wait for SIGINT or (on Unix) SIGTERM.
async fn wait_for_termination() {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = &mut ctrl_c => {}
                    _ = terminate.recv() => {}
                }
                return;
            }
            Err(error) => {
                warn!("⚠️ Failed to install the SIGTERM handler: {error}");
            }
        }
    }

    let _ = ctrl_c.await;
}

/// Initialize the logging system from the configuration.
///
/// Sets up tracing with layered output: a rotating file under
/// `[logging] file_path` and, optionally, an ANSI coloured console layer on
/// `stdout`. Which layers are installed and how verbose they are come from the
/// `[logging]` section, so the configuration file is the single place an
/// operator has to change.
///
/// The level is resolved by layering, highest priority first: `RUST_LOG` when
/// it is set (operational override), an explicit `--log-level` / `--debug`
/// from the command line, `[logging] level` from the configuration file, and
/// finally the built-in [`LogLevel::Info`]. Keeping the command line ahead of
/// the file is what makes the flag a real override.
///
/// # Arguments
///
/// * `config` - The `[logging]` section of the effective configuration.
/// * `cli_level` - Verbosity explicitly requested with `--log-level` or
///   `--debug`, or `None` to defer to the configuration file. `--debug` is
///   reported by [`CliArgs::log_level_override`].
/// * `console` - Whether a console layer is permitted at all. This must be
///   `false` while the full screen TUI owns the terminal, because any write to
///   `stdout` would corrupt the rendered interface.
///
/// # Returns
///
/// Returns the [`tracing_appender::non_blocking::WorkerGuard`] that keeps the
/// non-blocking file writer alive, or `None` when file logging is disabled.
/// The caller must keep it in scope for the whole program; dropping it flushes
/// and stops file logging.
///
/// # Errors
///
/// Returns an error when the log file appender cannot be created or the
/// subscriber cannot be installed.
fn init_logging(
    config: &meta_text::config::LoggingConfig,
    cli_level: Option<LogLevel>,
    console: bool,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use std::path::{Path, PathBuf};

    // Precedence: `RUST_LOG`, then an explicit `--log-level` / `--debug`, then
    // `[logging] level`, then the built-in default.
    let configured = cli_level
        .or_else(|| LogLevel::parse(&config.level))
        .unwrap_or(LogLevel::Info);
    let build_filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(configured.as_filter_str()))
    };

    // --- optional file layer -------------------------------------------------
    let (file_layer, guard) = if config.enable_file {
        // Split the configured path into a directory and a file name prefix,
        // because the rolling appender derives `{prefix}.{date}` itself.
        let path = Path::new(config.file_path.trim());
        let directory = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let prefix = path.file_name().map_or_else(
            || "meta-text.log".to_string(),
            |name| name.to_string_lossy().into_owned(),
        );

        let mut builder = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(prefix);
        if config.max_files > 0 {
            // `0` would mean "delete everything"; the validator rejects it, and
            // this guard keeps the appender safe even if it is bypassed.
            builder = builder.max_log_files(config.max_files);
        }

        let appender = builder.build(&directory).with_context(|| {
            format!("Failed to open the log file under {}", directory.display())
        })?;
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);
        (
            Some(fmt::layer().with_writer(non_blocking).with_ansi(false)),
            Some(guard),
        )
    } else {
        (None, None)
    };

    // --- optional console layer ---------------------------------------------
    let console_layer = if console && config.enable_console {
        Some(fmt::layer().with_writer(std::io::stdout).with_ansi(true))
    } else {
        None
    };

    let console_enabled = console_layer.is_some();
    tracing_subscriber::registry()
        .with(build_filter())
        .with(file_layer)
        .with(console_layer)
        .init();

    info!(
        "📊 Logging initialized (level={}, file={}, console={})",
        configured.as_filter_str(),
        config.enable_file,
        console_enabled
    );
    if config.enable_file {
        info!(
            "📁 Log files are written as {}.YYYY-MM-DD (keeping {} file(s))",
            config.file_path, config.max_files
        );
    }
    // Note: `rotation_size_mb` is accepted for forward compatibility but not
    // enforced, because the appender rotates by time only.
    if config.rotation_size_mb != 10 {
        info!("ℹ️ logging.rotation_size_mb is not enforced by the time based appender");
    }

    // Hand the guard back to the caller so that it lives for the whole process.
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the logging system initializes without errors
    #[tokio::test]
    async fn test_logging_initialization() -> Result<()> {
        use meta_text::config::LoggingConfig;

        // Note: In real tests, we might want to use a test-specific logging setup
        // to avoid interfering with other tests. File logging is disabled here so
        // the test does not depend on the working directory being writable.
        let mut config = LoggingConfig::default();
        config.enable_file = false;

        let _guard = init_logging(&config, Some(LogLevel::Info), false)
            .context("Logging initialization should succeed")?;

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

    /// The headless core mode can be selected and validates
    #[tokio::test]
    async fn test_core_mode_arguments() {
        let args = CliArgs::try_parse_from([
            "meta-text",
            "--mode",
            "core",
            "--ipc-listen",
            "127.0.0.1:45999",
            "--ipc-token",
            "s3cret",
        ])
        .expect("valid core invocation");

        assert_eq!(args.effective_mode(), AppMode::Core);
        assert_eq!(args.ipc_listen.as_deref(), Some("127.0.0.1:45999"));
        assert_eq!(args.ipc_token.as_deref(), Some("s3cret"));
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
