/*!
 * lib.rs
 *
 * metaText subsystems: everything the core service drives, and nothing that
 * drives the core service.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * # Layering
 *
 * This crate sits directly above [`meta_text_proto`] and below the service:
 *
 * - [`crypto`]: authenticated encryption and key derivation
 * - [`database`]: `SQLite` persistence for contacts and messages
 * - [`network`]: peer to peer TCP transport
 * - [`identity`]: the persistent X25519 identity a peer derives the pair's key
 *   from, and the file that makes it outlive a restart
 * - [`trust`]: trust on first use for a peer's announced identity, so a change
 *   under a known nickname is reported
 * - `tox`: optional Tox transport over the system `libtoxcore`, only compiled
 *   with the `tox-protocol` feature
 * - [`transport`]: the transport abstraction the core actor drives, so the same
 *   backend serves either TCP or Tox
 * - [`tox_store`]: the file the Tox transport persists its pending friend requests
 *   and outbox to, so a restart does not lose either (compiled with the `tox`
 *   module, under the same `tox-protocol` feature)
 * - [`config`]: TOML based configuration management
 * - [`logging`]: tracing setup and the size rotating file appender
 * - [`cli`]: command line vocabulary (`CliArgs`, `Transport`, `LogLevel`)
 *
 * The service crate depends on this one; this one never depends on the service,
 * on a presentation crate or on the binary. `error`, `types` and `utils` are
 * re-exported from [`meta_text_proto`] so that a subsystem can name them without
 * a second path.
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

pub mod cli;
pub mod config;
pub mod crypto;
pub mod database;
pub mod identity;
pub mod logging;
pub mod network;
#[cfg(feature = "tox-protocol")]
pub mod tox;
#[cfg(feature = "tox-protocol")]
pub mod tox_store;
pub mod transport;
pub mod trust;

pub use meta_text_proto::{error, types, utils};
pub use transport::{CoreTransport, PeerRequest};

/// Current version of the metaText backend crate.
///
/// # Examples
///
/// ```rust
/// assert_eq!(meta_text_backend::VERSION, env!("CARGO_PKG_VERSION"));
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
