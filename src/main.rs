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
    // `clippy::cargo` minus `multiple_crate_versions`: the duplicate
    // `wasi`/`getrandom` versions arrive transitively (SQLx and uuid pin
    // different majors) and are unreachable from our code, so the lint only
    // reports a dependency's choice. The metadata lints stay on — they are what
    // caught the member crates' missing README/keywords/categories.
    clippy::cargo_common_metadata,
    clippy::negative_feature_names,
    clippy::redundant_feature_names,
    clippy::wildcard_dependencies,
    // Panic-prone constructs are rejected in the shipped paths: a fault travels as
    // an error value, not as an unwind, because a front end that panics takes the
    // session (and the user's terminal) with it. CI turns warnings into errors, so
    // this list is a gate rather than a suggestion.
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::unwrap_used,
    rust_2018_idioms
)]
// A test may assert by unwrapping: the gate above guards the shipped paths.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use meta_text::cli::{AppMode, CliArgs, LogLevel, Transport};
use meta_text::config::AppConfig;
use meta_text::ipc::client::LocalClient;
use meta_text::ipc::server::{CoreServer, ServerOptions};
use meta_text::ipc::{CoreHandle, CoreService};
use meta_text::tui::{Theme, UiOptions};
use meta_text::ui::{CliFrontend, TuiFrontend};

/// Translate the `[ui]` section into the preferences the interface honours.
///
/// The naming differs on purpose: the configuration describes what a user asked
/// for, while [`UiOptions`] is the smaller set of decisions the presentation layer
/// actually makes. `message_format` is not among them — the configuration refuses a
/// value it cannot keep rather than accepting it here and ignoring it.
fn ui_options(ui: &meta_text::config::UiConfig) -> UiOptions {
    UiOptions {
        colors: ui.enable_colors,
        theme: if ui.theme.trim().eq_ignore_ascii_case("light") {
            Theme::Light
        } else {
            Theme::Dark
        },
        mouse: ui.enable_mouse,
        auto_scroll: ui.auto_scroll,
    }
}

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

    // The transport is chosen by the core itself, so every front-end works the
    // same way over TCP and Tox: `--transport tox` only changes which transport
    // `CoreService` builds.
    if !matches!(args.transport, Transport::Tcp | Transport::Tox) {
        bail!("unsupported transport: {}", args.transport);
    }

    // Build and start the backend, then detach it onto its own task.
    // The endpoint policy is copied out first: the core takes ownership of the
    // configuration, and a headless run needs the `[ipc]` section afterwards.
    let ipc = config.ipc.clone();
    // The presentation layer is written against the core protocol, so the `[ui]`
    // settings it honours are translated here, before the configuration is handed
    // to the core: the composition root is the only place that knows both.
    let ui_options = ui_options(&config.ui);

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

    match run_interface(&core, &args, mode, &app_name, &ipc, ui_options).await {
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
    ipc: &meta_text::config::IpcConfig,
    ui_options: UiOptions,
) -> Result<()> {
    let client = LocalClient::new(core.clone());

    match mode {
        AppMode::Tui if !args.headless => {
            let mut frontend = TuiFrontend::start(client, app_name, true, ui_options)
                .await
                .context("Failed to start the terminal interface")?;
            frontend.run().await
        }
        AppMode::Cli if !args.headless => {
            let mut frontend = CliFrontend::new(client);
            frontend.run().await
        }
        // The headless modes (`server`, `daemon`, `core`) never touch the
        // terminal, and `--headless` routes every other mode here as well.
        _ => run_headless(core, args, ipc).await,
    }
}

/// Host the backend without a user interface.
///
/// When `--ipc-listen` is given the core protocol is exposed on that address so
/// other front-ends can attach; otherwise the process simply idles until it
/// receives SIGINT or SIGTERM.
async fn run_headless(
    core: &CoreHandle,
    args: &CliArgs,
    ipc: &meta_text::config::IpcConfig,
) -> Result<()> {
    let server = match args.ipc_listen.as_deref() {
        Some(address) => {
            // An exposed endpoint must be authenticated. Loopback is a local
            // trust decision; anything else without a token would let any host
            // on the network drive the backend.
            if args.ipc_token.is_none() && !meta_text::utils::is_loopback_host(address) {
                bail!(
                    "--ipc-listen {address} is not a loopback address and no --ipc-token was \
                     given: refusing to expose an unauthenticated core endpoint"
                );
            }

            let endpoint = CoreServer::bind(address)
                .await
                .with_context(|| format!("Failed to bind the core protocol to {address}"))?;
            if let Ok(local) = endpoint.local_addr() {
                info!("🛰️ Core protocol endpoint bound to {local}");
            }
            // The policy comes from `[ipc]`, with the flags as per-run overrides:
            // the file is where an operator changes it, the flag is what a one-off
            // `--ipc-listen` uses.
            let options = ServerOptions {
                token: args.ipc_token.clone(),
                requests_per_second: args.ipc_rate.unwrap_or(ipc.requests_per_second),
                request_burst: args.ipc_burst.unwrap_or(ipc.request_burst),
                ..ServerOptions::default()
            };
            info!(
                "🚦 Serving at most {} request(s)/s per client address (burst {})",
                options.requests_per_second, options.request_burst
            );
            if options.token.is_none() {
                warn!(
                    "⚠️ --ipc-listen on a loopback address without --ipc-token: \
                     any local process can attach"
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
        // `rotation_size_mb` is enforced by the writer rather than accepted and
        // ignored: `tracing_appender`'s rolling appender rotates by time only.
        let (mut appender, directory) = meta_text::logging::file_writer(config);
        // Open the first file eagerly: `non_blocking` swallows write errors into its
        // own thread, so a log directory that cannot be created would otherwise lose
        // every line silently. This turns it into a startup error naming the path.
        appender.open().with_context(|| {
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
            "📁 Log files are written as {}.YYYY-MM-DD (rolling at {} MB, keeping {} file(s))",
            config.file_path, config.rotation_size_mb, config.max_files
        );
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
        let config = LoggingConfig {
            enable_file: false,
            ..LoggingConfig::default()
        };

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

        // Port 0 is the documented "let the OS pick a free port" case.
        let ephemeral = CliArgs::try_parse_from(["meta-text", "--port", "0"]).expect("parses");
        assert!(ephemeral.validate().is_ok());
        assert_eq!(ephemeral.port, Some(0));

        // A port outside `u16` is refused by the parser itself.
        assert!(CliArgs::try_parse_from(["meta-text", "--port", "70000"]).is_err());

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
