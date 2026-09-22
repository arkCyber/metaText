/*!
 * lib.rs
 *
 * metaText core service: the headless backend a user interface attaches to.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * # Layering
 *
 * ```text
 *   meta-text-cli   meta-text-tui     presentation (other crates)
 *      \          /
 *       \        /  ipc protocol
 *        +------+
 *        | core |  backend: crypto, storage, transport, contacts
 *        +------+
 * ```
 *
 * This crate is the only thing a presentation crate needs:
 *
 * - [`ipc::core`]: the actor (`CoreService` / `CoreHandle`) and its options
 * - [`ipc::client`]: the contract a front-end is written against, with the
 *   in-process and TCP implementations
 * - [`ipc::server`]: the endpoint that exposes the actor to out-of-process UIs
 * - [`config`], [`cli`], [`logging`]: re-exported from `meta-text-backend`,
 *   because a caller configures the service with them
 *
 * The subsystems (`crypto`, `database`, `network`, `transport`, `tox`) live in
 * `meta-text-backend` and are deliberately **not** re-exported here: a front-end
 * that depends on this crate cannot name a key, a row or a socket, which is the
 * mechanical form of §2.1 of the architecture document.
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

pub mod ipc;

pub use meta_text_backend::{cli, config, logging};

/// Subsystems the actor drives. Private on purpose: the only way a front-end
/// reaches the backend is [`ipc::client`].
#[allow(unused_imports)]
pub(crate) use meta_text_backend::{crypto, database, identity, network, transport};

pub use meta_text_proto::{error, types, utils};

/// Current version of the metaText core service crate.
///
/// # Examples
///
/// ```rust
/// assert_eq!(meta_text_core::VERSION, env!("CARGO_PKG_VERSION"));
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
