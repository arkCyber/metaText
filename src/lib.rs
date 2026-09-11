/*!
 * lib.rs
 *
 * metaText library crate - a Web3 decentralized instant messaging client
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - Modular architecture exposed as a reusable library
 * - End-to-end encrypted messaging primitives
 * - Decentralized P2P networking building blocks
 * - Persistent storage abstractions
 * - Configuration and error handling utilities
 *
 * # Overview
 *
 * `meta_text` is the core library behind the `meta-text` binary. It is split
 * into small, focused modules so that each subsystem can be tested and reused
 * independently:
 *
 * - [`ipc`]: the versioned interface between the user interfaces and the core
 * - [`ipc::core`]: the headless backend service that owns all domain state
 * - [`ui`]: the CLI and TUI front-ends (presentation only)
 * - [`cli`]: command line argument parsing
 * - [`commands`]: interactive slash command parsing
 * - [`config`]: TOML based configuration management
 * - [`crypto`]: authenticated encryption and key derivation
 * - [`database`]: SQLite persistence for contacts and messages
 * - [`network`]: peer to peer TCP transport
 * - [`tui`]: terminal rendering engine and console output routing
 * - [`types`]: shared domain types
 * - [`utils`]: small helper functions
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
    clippy::cargo,
    rust_2018_idioms
)]

pub mod cli;
pub mod commands;
pub mod config;
pub mod crypto;
pub mod database;
pub mod error;
pub mod ipc;
pub mod network;
pub mod tui;
pub mod types;
pub mod ui;
pub mod utils;

pub use cli::{AppMode, CliArgs, LogLevel};
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
