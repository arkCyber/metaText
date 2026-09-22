/*!
 * lib.rs
 *
 * metaText umbrella crate - a Web3 decentralized instant messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * This package owns the `meta-text` binary (the composition root) and re-exports
 * the workspace layers under the single `meta_text` name, so an application can
 * depend on one crate while the layers stay separately testable and separately
 * compilable.
 *
 * # Workspace layers
 *
 * ```text
 *   meta-text-proto    wire contract + shared vocabulary   (bottom)
 *     `- meta-text-backend   crypto, database, network, tox, transport
 *          `- meta-text-core   CoreService / CoreHandle
 *               `- meta-text-tui   presenter + rendering engine
 *                    `- meta-text-cli   the REPL front-end
 *                         `- meta-text   this crate: binary + umbrella
 * ```
 *
 * - [`ipc`]: the versioned interface between the user interfaces and the core
 * - [`ui`]: the CLI and TUI front-ends (presentation only)
 * - [`cli`]: command line argument parsing
 * - [`commands`]: interactive slash command parsing
 * - [`config`]: TOML based configuration management
 * - [`crypto`]: authenticated encryption and key derivation
 * - [`database`]: `SQLite` persistence for contacts and messages
 * - [`network`]: peer to peer TCP transport
 * - [`identity`]: the persisted X25519 identity an instance announces, and the
 *   file that keeps it stable across restarts
 * - [`trust`]: what each peer announced, pinned so a change under a known
 *   nickname is reported
 * - `tox`: optional Tox transport bindings over the system `libtoxcore`, only
 *   compiled with the `tox-protocol` feature
 * - [`transport`]: the transport abstraction the core actor drives, so the same
 *   backend serves either TCP or Tox
 * - [`tui`]: terminal rendering engine and console output routing
 * - [`types`]: shared domain types
 * - [`utils`]: small helper functions
 *
 * The subsystems are re-exported here for the binary and for the test suite;
 * a front-end that wants the boundary enforced by the compiler depends on
 * `meta-text-core` directly, which re-exports only the service, the client
 * contract and the configuration vocabulary.
 *
 * # Examples
 *
 * ```rust
 * use meta_text::{config::AppConfig, types::AppState};
 *
 * let config = AppConfig::default();
 * assert_eq!(config.app.name, "metaText");
 *
 * let state = AppState::new();
 * assert_eq!(state.statistics.messages_sent, 0);
 * ```
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

// ---------------------------------------------------------------------------
// Shared vocabulary (`meta-text-proto`)
// ---------------------------------------------------------------------------
pub use meta_text_proto::{error, types, utils};

// ---------------------------------------------------------------------------
// Subsystems (`meta-text-backend`)
// ---------------------------------------------------------------------------
#[cfg(feature = "tox-protocol")]
pub use meta_text_backend::tox;
pub use meta_text_backend::transport::{CoreTransport, PeerRequest};
pub use meta_text_backend::{
    cli, config, crypto, database, identity, logging, network, transport, trust,
};

// ---------------------------------------------------------------------------
// Presentation (`meta-text-tui`, `meta-text-cli`)
// ---------------------------------------------------------------------------
pub use meta_text_tui::{commands, tui};

/// Interface boundary between the metaText user interfaces and the core
/// application service, as exposed by `meta-text-core`.
pub mod ipc {
    pub use meta_text_core::ipc::*;
}

/// Presentation layer: the CLI and TUI front-ends and their shared logic.
pub mod ui {
    pub use meta_text_cli::ui::*;
    pub use meta_text_tui::presenter::{self, Presenter};
    pub use meta_text_tui::text;
    pub use meta_text_tui::ui::*;
}

pub use cli::{AppMode, CliArgs, LogLevel, Transport, TOX_ADDRESS_HEX_LEN};
pub use config::AppConfig;
pub use error::{MetaTextError, MetaTextResult};
pub use ipc::{CoreHandle, CoreService};
pub use ui::{CliFrontend, Presenter, TuiFrontend};

/// Current version of the metaText library
///
/// # Examples
///
/// ```rust
/// assert_eq!(meta_text::VERSION, env!("CARGO_PKG_VERSION"));
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
